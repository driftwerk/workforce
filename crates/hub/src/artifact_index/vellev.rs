//! Artifact vectors in vellev, embedded by the node's own bge.
//!
//! The collection is made sure of before its first use, and again before
//! each use while the node does not answer. One an operator has to put
//! right is refused: of the wrong shape, as one vellev's importer copied
//! from Weaviate with its vectors is, in a tenant the key does not reach,
//! or on a node that cannot run the model. It is looked at again every
//! [`RECHECK_AFTER`], so one put right is used without a restart.
//!
//! A collection newly made holds no artifact until a backfill has indexed
//! them all. A marker records when one has, and names the making of the
//! collection it was complete in ([`workforce_vellev::identity`]): a
//! collection dropped, or left out of a restored backup, is another when
//! it is made again, and no marker is of it.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex;
use tracing::warn;
use vellev_client::api::{Content, Nearest, Page};
use vellev_client::{Records, Tenant};
use vellev_core::{Collection, CollectionName, DataType, ObjectId, Property};
use workforce_common::vectorizable::VectorizableText;
use workforce_vellev::{
    collection, local_embedding, Cap, Ensured, Timeouts, Unusable, RECHECK_AFTER,
};

use super::{object_id, ArtifactVectors, COLLECTION};

/// How long the keeper of the index gives one try at one artifact on a
/// vellev node, whatever the node's client would wait (it reads for two
/// minutes, and tries again). Measured on a node embedding with its own
/// model on a laptop, nothing else asking of it: a write took 57 to 195 ms
/// for half a kilobyte of text and 330 to 671 ms for half a megabyte,
/// which the node cuts to what its model reads. Thirty seconds is some
/// forty times the slowest of those, for a node that is embedding a
/// repo's code at the same time, which was not measured.
pub const TRY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// What to do about an artifacts collection the node cannot use: artifacts
/// are indexed again from Postgres, by the hub that serves the index when
/// there is one (it finds the collection made again and empty, and fills
/// it), and by `workforce artifacts reindex` when none does.
pub const REINDEX: &str = "drop it; a hub serving this index then makes it again and fills it from Postgres, and for an index no hub serves `workforce artifacts reindex` does";

/// How long boot waits to learn whether the collection can be used before
/// going on, making sure of it on first use instead.
const USABLE_WAIT: Duration = Duration::from_secs(10);

/// Objects per page of a listing.
const LIST_PAGE: u32 = 500;

/// The property an artifact's text hash is kept in.
const TEXT_HASH: &str = "textHash";

/// How a [`VellevArtifacts`] paces itself, for tests to shorten.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub timeouts: Timeouts,
    /// How long the collection's making and its marker are taken as last
    /// read.
    pub recheck_after: Duration,
    /// Objects per page of a listing.
    pub list_page: u32,
}

impl Pace {
    /// The pace of shadow mode's reads of the artifact mirror (DW-676,
    /// A7): one attempt, given up at `deadline`, the kernel's own, so that
    /// a shadow read holds its permit no longer than that and is timed as
    /// one request.
    pub fn shadow_read(deadline: Duration) -> Self {
        Self {
            timeouts: Timeouts {
                read: deadline,
                attempts: 1,
                ..Timeouts::default()
            },
            ..Self::default()
        }
    }
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            timeouts: Timeouts::default(),
            recheck_after: RECHECK_AFTER,
            list_page: LIST_PAGE,
        }
    }
}

/// The `Artifacts` collection of a vellev tenant.
pub struct VellevArtifacts {
    tenant: Tenant,
    artifacts: Ensured,
    /// The collection as writes the node embeds reach it, given longer.
    writes: Records,
    cap: Cap,
    /// The making of the collection whose marker was last seen, and when:
    /// search need not look for it again for `recheck_after`.
    marked: Mutex<Option<(String, Instant)>>,
    /// The making of the collection a backfill began in.
    backfilling: Mutex<Option<String>>,
    recheck_after: Duration,
    list_page: u32,
    tenants: workforce_vellev::Tenants,
    /// When set, an artifact's vector is made outside the node and sent
    /// with it.
    seeding: Option<workforce_vellev::seeding::Seeding>,
}

impl VellevArtifacts {
    /// The artifacts of `tenant` on the node at `url`, reached with
    /// `api_key`; every request is bounded by the client's timeout.
    pub fn new(url: &str, api_key: &str, tenant: &str) -> eyre::Result<Self> {
        Self::paced(url, api_key, tenant, Pace::default())
    }

    /// As [`Self::new`], at `pace`.
    pub fn paced(url: &str, api_key: &str, tenant: &str, pace: Pace) -> eyre::Result<Self> {
        let Pace {
            timeouts,
            recheck_after,
            list_page,
        } = pace;
        let tenants = workforce_vellev::connect_with(url, api_key, tenant, timeouts)?;
        let wanted = artifacts_collection()?;
        Ok(Self {
            writes: tenants.writes.collection(&wanted.name),
            artifacts: Ensured::new(tenants.reads.clone(), wanted, REINDEX)
                .identified()
                .rechecking(recheck_after),
            tenant: tenants.reads.clone(),
            cap: Cap::default(),
            marked: Mutex::new(None),
            backfilling: Mutex::new(None),
            recheck_after,
            list_page,
            tenants,
            seeding: None,
        })
    }

    /// Have each artifact's vector made by `seeding`'s embedder and sent
    /// with it, rather than made by the node. Only for an embedder checked
    /// to embed as the node does (see
    /// [`workforce_vellev::seeding::equivalence`]).
    pub fn seeded_by(mut self, seeding: workforce_vellev::seeding::Seeding) -> Self {
        self.seeding = Some(seeding);
        self
    }

    /// The tenant, as reads and as writes reach it, and how artifacts are
    /// embedded: what an equivalence check needs.
    pub fn embedded_as(
        &self,
    ) -> eyre::Result<(&workforce_vellev::Tenants, vellev_core::Embedding)> {
        Ok((&self.tenants, artifacts_collection()?.embedding))
    }

    /// Create the collection, or check an existing one.
    pub async fn ensure_collection(&self) -> eyre::Result<()> {
        self.artifacts.records().await.map(|_| ())
    }

    /// `Err` if an operator has to act before the collection can be used
    /// ([`Unusable`]); otherwise `Ok`, even when the node does not answer
    /// within [`USABLE_WAIT`], as the collection is then made sure of on
    /// first use.
    pub async fn usable(&self) -> eyre::Result<()> {
        match tokio::time::timeout(USABLE_WAIT, self.ensure_collection()).await {
            Ok(Err(e)) if e.downcast_ref::<Unusable>().is_some() => Err(e),
            Ok(Err(e)) => {
                warn!(error = ?e, "vellev did not answer for artifacts; their collection is made sure of on first use");
                Ok(())
            }
            Err(_) => {
                warn!(wait = ?USABLE_WAIT, "vellev did not answer for artifacts in time; their collection is made sure of on first use");
                Ok(())
            }
            Ok(Ok(())) => Ok(()),
        }
    }

    /// The collection as it is now, looked at afresh: made again if it is
    /// gone, and then another.
    async fn making(&self) -> eyre::Result<String> {
        self.artifacts.recheck().await?;
        self.artifacts.identity().await
    }
}

/// What the marker of one making of the collection is kept under.
fn marker(making: &str) -> String {
    format!("{COLLECTION}@{making}")
}

pub(super) fn artifacts_collection() -> eyre::Result<Collection> {
    Ok(collection(
        CollectionName::new(COLLECTION)?,
        vec![
            Property::new("artifactId", DataType::Text),
            Property {
                filterable: false,
                ..Property::new("text", DataType::Text).searchable()
            },
            // The hash of the text, as the artifact store computes it:
            // what tells an index holding an earlier text from one that
            // agrees with Postgres.
            // Filterable, with the id: a page of artifacts is then
            // compared by one count, none of their text read.
            Property::new(TEXT_HASH, DataType::Text),
        ],
        local_embedding(&["text"]),
    ))
}

#[async_trait]
impl ArtifactVectors for VellevArtifacts {
    fn backend(&self) -> &'static str {
        "vellev"
    }

    async fn upsert(&self, artifact_id: &str, text: &str, text_hash: &str) -> eyre::Result<()> {
        let vector = match &self.seeding {
            Some(seeding) if VectorizableText::try_new(text).is_ok() => seeding
                .embedder
                .documents(&[text.to_string()])
                .await?
                .into_iter()
                .next(),
            _ => None,
        };
        let content = Content {
            properties: json!({ "artifactId": artifact_id, "text": text, TEXT_HASH: text_hash })
                .as_object()
                .cloned()
                .unwrap_or_default(),
            vector,
        };
        let id = ObjectId(object_id(artifact_id));
        let put = self.artifacts.run(|| self.writes.put(id, &content)).await;
        put.map(|_| ())
    }

    async fn remove(&self, artifact_id: &str) -> eyre::Result<()> {
        let id = ObjectId(object_id(artifact_id));
        self.artifacts
            .run(|| self.artifacts.collection().delete(id))
            .await?;
        Ok(())
    }

    /// Nothing is found for a query with nothing to embed.
    async fn search(&self, query: &str, limit: usize) -> eyre::Result<Vec<(String, f32)>> {
        let Ok(query) = VectorizableText::try_new(query) else {
            return Ok(Vec::new());
        };
        let found = self
            .artifacts
            .run(|| {
                self.cap.ask(limit, |limit| {
                    let nearest = Nearest {
                        vector: None,
                        text: Some(query.as_str().to_string()),
                        limit,
                        filter: None,
                        oversample: None,
                        vectors: false,
                    };
                    async move { self.artifacts.collection().nearest(&nearest).await }
                })
            })
            .await?;
        Ok(found
            .results
            .into_iter()
            .filter_map(|found| {
                let id = found.object.properties.get("artifactId")?.as_str()?;
                Some((id.to_string(), found.certainty.unwrap_or(0.0)))
            })
            .collect())
    }

    /// Complete once a backfill has marked this making of the collection
    /// so. Which making the node holds, and whether it is marked, are read
    /// from the node again every `recheck_after`, and at once after a
    /// request found the collection gone.
    async fn complete(&self) -> eyre::Result<bool> {
        let making = self.artifacts.identity().await?;
        // No lock is held while the node is asked: a node that does not
        // answer keeps one caller waiting, not every caller behind it.
        if let Some((seen, at)) = &*self.marked.lock().await {
            if *seen == making && at.elapsed() < self.recheck_after {
                return Ok(true);
            }
        }
        let complete = workforce_vellev::index_marked(&self.tenant, &marker(&making)).await?;
        *self.marked.lock().await = complete.then(|| (making, Instant::now()));
        Ok(complete)
    }

    async fn mark_incomplete(&self) -> eyre::Result<()> {
        let making = self.making().await?;
        workforce_vellev::index_unmark(&self.tenant, &marker(&making)).await?;
        *self.marked.lock().await = None;
        *self.backfilling.lock().await = Some(making);
        Ok(())
    }

    async fn mark_complete(&self) -> eyre::Result<()> {
        let making = self.making().await?;
        let began_in = self.backfilling.lock().await.clone();
        if began_in.as_deref() != Some(making.as_str()) {
            eyre::bail!(
                "the vellev collection '{COLLECTION}' was made again while it was being indexed, and holds only part"
            );
        }
        workforce_vellev::index_mark(&self.tenant, &marker(&making)).await?;
        *self.marked.lock().await = Some((making, Instant::now()));
        Ok(())
    }

    /// By one count over the id and the hash; only a page that differs
    /// is read, and only its own artifacts (see
    /// [`workforce_vellev::unmatched`]).
    async fn differing(&self, page: &[(String, String)]) -> eyre::Result<Vec<String>> {
        let found = self
            .artifacts
            .run(|| {
                workforce_vellev::unmatched(
                    self.artifacts.collection(),
                    None,
                    ("artifactId", TEXT_HASH),
                    page,
                    &self.cap,
                )
            })
            .await?;
        Ok(found.differing)
    }

    async fn ids_after(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> eyre::Result<(Vec<String>, Option<String>)> {
        let after = match after {
            Some(after) => Some(ObjectId(after.parse()?)),
            None => None,
        };
        let listed = self
            .artifacts
            .run(|| {
                self.cap.ask(limit.min(self.list_page as usize), |limit| {
                    let page = Page {
                        filter: None,
                        after,
                        limit,
                        vectors: false,
                    };
                    async move { self.artifacts.collection().list(&page).await }
                })
            })
            .await?;
        let ids = listed
            .objects
            .iter()
            .filter_map(|object| Some(object.properties.get("artifactId")?.as_str()?.to_string()))
            .collect();
        Ok((ids, listed.next.map(|next| next.0.to_string())))
    }

    async fn count(&self) -> eyre::Result<usize> {
        let counted = self
            .artifacts
            .run(|| self.artifacts.collection().count(None))
            .await?;
        Ok(usize::try_from(counted)?)
    }
}

#[cfg(test)]
mod tests {
    use workforce_vellev::testing::Node;

    use super::*;

    /// Needs a node that runs the local model.
    #[tokio::test]
    async fn an_artifact_is_found_by_meaning_replaced_and_removed() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let artifacts = VellevArtifacts::new(&node.url, &node.key, &node.tenant).unwrap();
        artifacts.ensure_collection().await.unwrap();
        artifacts.ensure_collection().await.unwrap();
        artifacts
            .upsert(
                "spec-1",
                "Deploy runbook\n\ntags: ops\n\nHow to roll back a release",
                "h",
            )
            .await
            .unwrap();
        artifacts
            .upsert(
                "spec-2",
                "Lunch menu\n\ntags: food\n\nSoup and bread on Fridays",
                "h",
            )
            .await
            .unwrap();
        let found = artifacts.search("rolling back a deploy", 5).await.unwrap();
        assert_eq!(found[0].0, "spec-1", "{found:?}");
        assert!(found[0].1 > found[1].1 && found[0].1 <= 1.0);

        artifacts
            .upsert(
                "spec-1",
                "Bread recipe\n\ntags: food\n\nFlour, water, salt",
                "h",
            )
            .await
            .unwrap();
        assert_eq!(
            artifacts
                .artifacts
                .records()
                .await
                .unwrap()
                .count(None)
                .await
                .unwrap(),
            2,
            "replaced, not added"
        );
        let found = artifacts.search("baking bread", 5).await.unwrap();
        assert_eq!(found[0].0, "spec-1", "{found:?}");

        artifacts.remove("spec-1").await.unwrap();
        let found = artifacts.search("baking bread", 5).await.unwrap();
        assert!(found.iter().all(|(id, _)| id != "spec-1"), "{found:?}");
    }

    #[tokio::test]
    async fn a_query_with_nothing_to_embed_finds_nothing_without_asking() {
        let artifacts = VellevArtifacts::new("http://127.0.0.1:1", "key", "workforce").unwrap();
        for query in ["", "   ", "!?"] {
            assert!(artifacts.search(query, 5).await.unwrap().is_empty());
        }
    }

    /// Needs a node that runs the local model.
    #[tokio::test]
    async fn artifacts_on_a_node_that_comes_up_late_are_indexed_on_first_use() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let (url, port) = Node::not_up_yet().await;
        let artifacts = VellevArtifacts::paced(&url, &node.key, &node.tenant, at_once()).unwrap();
        artifacts
            .usable()
            .await
            .expect("kept, though the node did not answer");
        assert!(artifacts
            .upsert("spec-1", "Deploy runbook", "h")
            .await
            .is_err());
        node.come_up_at(port).await;
        artifacts
            .upsert("spec-1", "Deploy runbook", "h")
            .await
            .unwrap();
        assert_eq!(
            artifacts.search("deploying", 5).await.unwrap()[0].0,
            "spec-1"
        );
    }

    #[tokio::test]
    async fn an_artifacts_collection_copied_from_weaviate_is_refused() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenant = node.tenant();
        let mut imported = artifacts_collection().unwrap();
        imported.embedding = vellev_core::Embedding {
            provider: vellev_core::EmbeddingProvider::None,
            model: "weaviate:text2vec-transformers".into(),
            dimensions: 384,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        };
        tenant
            .collection(&imported.name)
            .create(&imported)
            .await
            .unwrap();
        let refused = VellevArtifacts::new(&node.url, &node.key, &node.tenant)
            .unwrap()
            .usable()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("copied from Weaviate") && refused.contains(REINDEX),
            "{refused}"
        );
    }

    /// A key vellev does not accept, or a tenant it does not have, is for
    /// an operator to fix: no indexer, and search goes by keyword.
    #[tokio::test]
    async fn a_refused_key_or_missing_tenant_leaves_artifacts_unindexed() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let wrong_key = VellevArtifacts::new(&node.url, "no-such-key", &node.tenant).unwrap();
        let refused = wrong_key.usable().await.unwrap_err().to_string();
        assert!(refused.contains("does not accept the key"), "{refused}");
        let missing = format!("{}-missing", node.tenant);
        let no_tenant = VellevArtifacts::new(&node.url, &node.key, &missing).unwrap();
        let refused = no_tenant.usable().await.unwrap_err().to_string();
        assert!(refused.contains(&missing), "{refused}");
    }

    /// A node that accepts the connection and never answers does not hold
    /// up building the indexer beyond its wait.
    #[tokio::test]
    async fn a_node_that_never_answers_holds_up_the_indexer_no_longer_than_its_wait() {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", silent.local_addr().unwrap());
        let artifacts = VellevArtifacts::new(&url, "key", "workforce").unwrap();
        let started = std::time::Instant::now();
        artifacts.usable().await.unwrap();
        assert!(started.elapsed() < USABLE_WAIT + Duration::from_secs(2));
        drop(silent);
    }

    /// Shadow mode's reads make one attempt, each within the kernel's
    /// deadline (A7): a shadow read is timed as one request, and holds
    /// its permit no longer than the deadline.
    #[test]
    fn a_shadow_read_is_one_attempt_within_the_deadline() {
        let pace = Pace::shadow_read(Duration::from_millis(700));
        assert_eq!(pace.timeouts.attempts, 1);
        assert_eq!(pace.timeouts.read, Duration::from_millis(700));
        assert_eq!(
            Pace::default().timeouts.attempts,
            4,
            "every other read keeps its tries"
        );
    }

    /// Looking at the node every time, as after the recheck time.
    fn at_once() -> Pace {
        Pace {
            recheck_after: Duration::ZERO,
            ..Pace::default()
        }
    }

    /// A new collection is incomplete until a backfill marks it; the mark
    /// outlives the store; and the collection made again, as after it was
    /// dropped or left out of a restored backup, is another, which no
    /// mark is of.
    #[tokio::test]
    async fn a_collection_is_complete_once_marked_and_no_longer_once_made_again() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let open =
            || VellevArtifacts::paced(&node.url, &node.key, &node.tenant, at_once()).unwrap();
        let artifacts = open();
        assert!(!artifacts.complete().await.unwrap());
        artifacts.mark_incomplete().await.unwrap();
        assert!(!artifacts.complete().await.unwrap());
        artifacts.mark_complete().await.unwrap();
        assert!(artifacts.complete().await.unwrap());
        assert!(
            open().complete().await.unwrap(),
            "the mark outlives the store"
        );

        artifacts.mark_incomplete().await.unwrap();
        assert!(!artifacts.complete().await.unwrap(), "unmarked");
        assert!(!open().complete().await.unwrap());
        artifacts.mark_complete().await.unwrap();
        assert!(artifacts.complete().await.unwrap());
        open().mark_incomplete().await.unwrap();
        assert!(
            !artifacts.complete().await.unwrap(),
            "unmarked by another store, seen once the recheck time is over"
        );
        artifacts.mark_incomplete().await.unwrap();
        artifacts.mark_complete().await.unwrap();

        // Dropped behind every store's back, while none was running.
        node.tenant()
            .collection(&CollectionName::new(COLLECTION).unwrap())
            .drop()
            .await
            .unwrap();
        assert!(
            !open().complete().await.unwrap(),
            "made again, it is another: the mark is of the one that was"
        );
        assert!(!artifacts.complete().await.unwrap());
    }

    /// A mark seen is not looked for again until the recheck time, unless
    /// a request finds the collection gone.
    #[tokio::test]
    async fn a_mark_seen_is_trusted_until_the_recheck_time_or_the_collection_is_found_gone() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let artifacts = VellevArtifacts::new(&node.url, &node.key, &node.tenant).unwrap();
        artifacts.mark_incomplete().await.unwrap();
        artifacts.mark_complete().await.unwrap();
        let other = VellevArtifacts::paced(&node.url, &node.key, &node.tenant, at_once()).unwrap();
        other.mark_incomplete().await.unwrap();
        assert!(
            artifacts.complete().await.unwrap(),
            "unmarked by another store, not seen before the recheck time"
        );
        other.mark_complete().await.unwrap();

        node.tenant()
            .collection(&CollectionName::new(COLLECTION).unwrap())
            .drop()
            .await
            .unwrap();
        assert!(artifacts.complete().await.unwrap(), "not looked at yet");
        assert!(artifacts.search("anything", 3).await.unwrap().is_empty());
        assert!(
            !artifacts.complete().await.unwrap(),
            "the search found the collection gone"
        );
    }

    /// A backfill's mark is refused when the collection it began in is no
    /// more, or when no backfill began.
    #[tokio::test]
    async fn a_collection_made_again_during_a_backfill_is_not_marked_complete() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let artifacts =
            VellevArtifacts::paced(&node.url, &node.key, &node.tenant, at_once()).unwrap();
        let unbegun = artifacts.mark_complete().await.unwrap_err().to_string();
        assert!(unbegun.contains("holds only part"), "{unbegun}");

        artifacts.mark_incomplete().await.unwrap();
        artifacts.artifacts.collection().drop().await.unwrap();
        let lost = artifacts.mark_complete().await.unwrap_err().to_string();
        assert!(lost.contains("made again"), "{lost}");
        assert!(!artifacts.complete().await.unwrap());
    }

    /// Every artifact id is listed, page after page, and counted.
    #[tokio::test]
    async fn ids_are_listed_page_by_page_and_counted() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let pace = Pace {
            list_page: 2,
            ..Pace::default()
        };
        let artifacts = VellevArtifacts::paced(&node.url, &node.key, &node.tenant, pace).unwrap();
        assert_eq!(
            artifacts.ids_after(None, 10).await.unwrap(),
            (Vec::new(), None)
        );
        assert_eq!(artifacts.count().await.unwrap(), 0);
        for n in 0..5 {
            artifacts
                .upsert(&format!("spec-{n}"), &format!("Runbook number {n}"), "h")
                .await
                .unwrap();
        }
        let mut ids = Vec::new();
        let (mut after, mut pages) = (None, 0);
        loop {
            let (page, next) = artifacts.ids_after(after, 100).await.unwrap();
            assert!(page.len() <= 2, "a page at a time");
            ids.extend(page);
            pages += 1;
            after = next;
            if after.is_none() {
                break;
            }
        }
        ids.sort();
        assert_eq!(ids, ["spec-0", "spec-1", "spec-2", "spec-3", "spec-4"]);
        assert!(pages >= 3, "{pages} pages");

        // Compared by id and hash: what is held as wanted costs nothing
        // to tell, and what is not is named.
        let wanted = |hashes: [&str; 3]| -> Vec<(String, String)> {
            ["spec-0", "spec-1", "spec-9"]
                .iter()
                .zip(hashes)
                .map(|(id, hash)| (id.to_string(), hash.to_string()))
                .collect()
        };
        assert_eq!(
            artifacts.differing(&wanted(["h", "h", "h"])).await.unwrap(),
            ["spec-9"]
        );
        assert_eq!(
            artifacts
                .differing(&wanted(["h", "other", "h"]))
                .await
                .unwrap(),
            ["spec-1", "spec-9"]
        );
        assert!(artifacts
            .differing(&wanted(["h", "h", "h"])[..2])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(artifacts.count().await.unwrap(), 5);
    }

    /// A fake node that has the collection, answers reads at once, and
    /// takes `slow` over storing an object.
    async fn slow_to_store(slow: Duration) -> String {
        use axum::response::IntoResponse;
        let mut held = artifacts_collection().unwrap();
        held.properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri| {
                let held = held.clone();
                async move {
                    let path = uri.path();
                    if path.contains("/objects/") && method == axum::http::Method::PUT {
                        tokio::time::sleep(slow).await;
                        let written = json!({ "incarnation": 1, "position": 1, "outcomes": [] });
                        return axum::Json(written).into_response();
                    }
                    if path.ends_with("/collections") {
                        return axum::Json(json!({ "collections": [] })).into_response();
                    }
                    if path.ends_with("/nearest") {
                        return axum::Json(json!({ "results": [] })).into_response();
                    }
                    axum::Json(held).into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// An artifact is written to the node by one request that replaces
    /// the object where it is: nothing is removed in order to be written,
    /// so a write the node does not take costs the index nothing it held.
    #[tokio::test]
    async fn a_write_asks_the_node_to_replace_and_to_remove_nothing() {
        use axum::response::IntoResponse;
        use std::sync::{Arc, Mutex};
        let asked = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let mut held = artifacts_collection().unwrap();
        held.properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let seen = Arc::clone(&asked);
        let app = axum::Router::new().fallback(
            move |method: axum::http::Method, uri: axum::http::Uri| {
                let (held, seen) = (held.clone(), Arc::clone(&seen));
                async move {
                    let path = uri.path().to_string();
                    if path.contains("/objects") {
                        seen.lock()
                            .unwrap()
                            .push((method.to_string(), path.clone()));
                    }
                    if path.ends_with("/collections") {
                        return axum::Json(json!({ "collections": [] })).into_response();
                    }
                    if path.ends_with("/nearest") {
                        return axum::Json(json!({ "results": [] })).into_response();
                    }
                    if path.contains("/objects") {
                        let problem = json!({ "error": { "code": "internal", "message": "why" } });
                        let failed = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                        return (failed, axum::Json(problem)).into_response();
                    }
                    axum::Json(held).into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let hasty = Pace {
            timeouts: Timeouts {
                read: Duration::from_millis(300),
                write: Duration::from_millis(300),
                ..Timeouts::default()
            },
            ..Pace::default()
        };
        let artifacts = VellevArtifacts::paced(&url, "key", "workforce", hasty).unwrap();
        assert!(artifacts.upsert("spec-1", "a text", "h").await.is_err());
        let asked = asked.lock().unwrap().clone();
        assert!(!asked.is_empty());
        let object = object_id("spec-1").to_string();
        for (method, path) in &asked {
            assert_eq!(method, "PUT", "{asked:?}");
            assert!(path.ends_with(&format!("/objects/{object}")), "{asked:?}");
        }
    }

    /// Gives vectors of a length no collection of the hub's has.
    struct Short;
    #[async_trait]
    impl workforce_vellev::seeding::DocumentEmbedder for Short {
        fn name(&self) -> String {
            "an embedder of the wrong length".to_string()
        }
        async fn documents(&self, texts: &[String]) -> eyre::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.5, 0.5, 0.5]).collect())
        }
    }

    /// And the node keeps what it held when it does not take a write: an
    /// artifact written again with a vector the node refuses is still
    /// there under the hash it had, and still found.
    #[tokio::test]
    async fn a_write_the_node_refuses_leaves_what_it_held() {
        let Some(node) = Node::embedding().await else {
            return;
        };
        let artifacts = VellevArtifacts::new(&node.url, &node.key, &node.tenant).unwrap();
        artifacts.ensure_collection().await.unwrap();
        let first = "Deploy runbook\n\nHow to roll back a release";
        artifacts.upsert("spec-1", first, "h1").await.unwrap();
        let held = [("spec-1".to_string(), "h1".to_string())];
        assert!(artifacts.differing(&held).await.unwrap().is_empty());

        let short = workforce_vellev::seeding::Seeding::new(std::sync::Arc::new(Short), 1, 1);
        let refused_by_node = VellevArtifacts::new(&node.url, &node.key, &node.tenant)
            .unwrap()
            .seeded_by(short);
        let told = refused_by_node.upsert("spec-1", "Lunch menu", "h2").await;
        assert!(told.is_err(), "a vector of three is not taken");
        assert!(
            artifacts.differing(&held).await.unwrap().is_empty(),
            "as it was"
        );
        assert_eq!(artifacts.count().await.unwrap(), 1);
        let found = artifacts.search("rolling back a deploy", 5).await.unwrap();
        assert_eq!(found[0].0, "spec-1", "{found:?}");
    }

    /// Storing an artifact, which the node embeds, is given the time of a
    /// write, not of a read.
    #[tokio::test]
    async fn storing_an_artifact_waits_as_long_as_a_write_may_take() {
        let url = slow_to_store(Duration::from_millis(600)).await;
        let pace = |write| Pace {
            timeouts: Timeouts {
                read: Duration::from_millis(200),
                write,
                ..Timeouts::default()
            },
            ..Pace::default()
        };
        let patient =
            VellevArtifacts::paced(&url, "key", "workforce", pace(Duration::from_secs(5))).unwrap();
        patient
            .upsert("spec-1", "Deploy runbook", "h")
            .await
            .expect("the node is given the time to embed");
        let hasty =
            VellevArtifacts::paced(&url, "key", "workforce", pace(Duration::from_millis(200)))
                .unwrap();
        assert!(hasty.upsert("spec-1", "Deploy runbook", "h").await.is_err());
    }

    /// A node that cannot run the model is refused loudly when the
    /// collection is first looked at, naming the model.
    #[tokio::test]
    async fn a_node_that_cannot_run_the_model_is_refused_naming_it() {
        use axum::response::IntoResponse;
        let mut held = artifacts_collection().unwrap();
        held.properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let held = held.clone();
            async move {
                let path = uri.path();
                if path.ends_with("/collections") {
                    return axum::Json(json!({ "collections": [] })).into_response();
                }
                if path.ends_with("/nearest") {
                    let refused = json!({ "error": {
                        "code": "model_unavailable",
                        "message": "the collection's embedding model cannot be used by this node"
                    }});
                    return (axum::http::StatusCode::NOT_IMPLEMENTED, axum::Json(refused))
                        .into_response();
                }
                axum::Json(held).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let artifacts = VellevArtifacts::new(&url, "key", "workforce").unwrap();
        let refused = artifacts.usable().await.unwrap_err();
        assert!(
            matches!(refused.downcast_ref(), Some(Unusable::Model { .. })),
            "{refused:?}"
        );
        let said = refused.to_string();
        assert!(
            said.contains("cannot run 'BAAI/bge-base-en-v1.5'") && said.contains("Artifacts"),
            "{said}"
        );
    }

    /// Callers asking at once whether the index is complete, of a node
    /// that does not answer for its marker, each wait their own request
    /// out side by side: none holds a lock the others queue behind.
    #[tokio::test]
    async fn callers_asking_whether_the_index_is_complete_do_not_queue() {
        use axum::response::IntoResponse;
        let mut held = artifacts_collection().unwrap();
        held.properties
            .push(Property::new("incarnation_1", DataType::Bool));
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let held = held.clone();
            async move {
                let path = uri.path();
                if path.contains("WorkforceIndexMarkers") {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                if path.ends_with("/collections") {
                    return axum::Json(json!({ "collections": [] })).into_response();
                }
                if path.ends_with("/nearest") {
                    return axum::Json(json!({ "results": [] })).into_response();
                }
                axum::Json(held).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let pace = Pace {
            timeouts: Timeouts {
                read: Duration::from_millis(150),
                write: Duration::from_millis(150),
                ..Timeouts::default()
            },
            ..Pace::default()
        };
        let artifacts =
            std::sync::Arc::new(VellevArtifacts::paced(&url, "key", "workforce", pace).unwrap());
        let began = std::time::Instant::now();
        assert!(artifacts.complete().await.is_err());
        let alone = began.elapsed();
        let began = std::time::Instant::now();
        let callers: Vec<_> = (0..6)
            .map(|_| {
                let artifacts = std::sync::Arc::clone(&artifacts);
                tokio::spawn(async move { artifacts.complete().await.is_err() })
            })
            .collect();
        for caller in callers {
            assert!(caller.await.unwrap());
        }
        assert!(
            began.elapsed() < alone * 2,
            "six callers took {:?}, one {alone:?}",
            began.elapsed()
        );
    }
}
