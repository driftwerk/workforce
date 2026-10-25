//! Tests of shadow mode's reads of memories (DW-676, PR 8).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use futures::FutureExt;
use workforce_common::shadow::{Comparison, Outcome, ShadowSink};
use workforce_memory::in_memory::InMemoryStore;
use workforce_memory::types::MemoryLayer;

use super::*;

fn agent() -> AgentId {
    AgentId::new("agent")
}

/// Embeds as the hub's model, counting the queries it embeds; failing
/// while told to.
#[derive(Default)]
struct Counting {
    queries: AtomicUsize,
    down: AtomicBool,
}

#[async_trait]
impl EmbeddingBackend for Counting {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(vec![text.len() as f32, 1.0])
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        match self.down.load(Ordering::SeqCst) {
            true => Err(eyre::eyre!("the embedder answered 503")),
            false => Ok(vec![text.len() as f32, 1.0]),
        }
    }
    fn model_id(&self) -> &str {
        "hub"
    }
}

/// How a store answers a recall.
#[derive(Default, Clone, Copy, PartialEq)]
enum Answers {
    #[default]
    Well,
    Never,
    Failing,
}

/// A store the hub embeds for, as Weaviate's and vellev's embedded stores
/// are: taking vectors of `model`, embedding the query itself (with
/// `embedder`) when not given one. It says what it was given and whether
/// it was asked as the shadow.
struct Vectored {
    inner: InMemoryStore,
    model: &'static str,
    embedder: Arc<Counting>,
    answers: Mutex<Answers>,
    given: Mutex<Vec<Option<Vec<f32>>>>,
    as_shadow: Mutex<Vec<bool>>,
    /// Each hit given twice, as a memory held in two objects can be.
    twice: bool,
}

impl Vectored {
    fn new(model: &'static str, embedder: Arc<Counting>) -> Self {
        Self {
            inner: InMemoryStore::new(),
            model,
            embedder,
            answers: Mutex::default(),
            given: Mutex::default(),
            as_shadow: Mutex::default(),
            twice: false,
        }
    }

    async fn answer(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        self.as_shadow
            .lock()
            .unwrap()
            .push(workforce_memory::access_audit::in_shadow());
        let answers = *self.answers.lock().unwrap();
        match answers {
            Answers::Never => std::future::pending().await,
            Answers::Failing => Err(eyre::eyre!("vellev answered 503")),
            Answers::Well => {
                let hits = self.inner.recall(query, filter).await?;
                Ok(match self.twice {
                    true => hits.iter().flat_map(|h| [h.clone(), h.clone()]).collect(),
                    false => hits,
                })
            }
        }
    }
}

#[async_trait]
impl MemoryStore for Vectored {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        self.inner.remember(memory).await
    }
    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        self.embedder.embed_query(query).await.ok();
        self.answer(query, filter).await
    }
    fn query_model(&self) -> Option<String> {
        Some(self.model.to_string())
    }
    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        self.given.lock().unwrap().push(vector);
        self.answer(query, filter).await
    }
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.inner.get(id).await
    }
    async fn forget(&self, id: &MemoryId) -> Result<()> {
        self.inner.forget(id).await
    }
    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        self.inner.share(id, visibility).await
    }
    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        self.inner.count(filter).await
    }
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
        self.inner.list(filter).await
    }
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        self.inner.update_tags(id, tags).await
    }
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        self.inner.adjust_importance(id, delta).await
    }
    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        self.inner.identity_memories(owner, identity).await
    }
    async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>> {
        self.inner.identity_counts(owner).await
    }
    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        self.inner.reassign_identity(owner, from, to).await
    }
    async fn purge_identity(&self, owner: &AgentId, identity: &str) -> Result<usize> {
        self.inner.purge_identity(owner, identity).await
    }
    async fn purge_memories(&self, ids: &[MemoryId]) -> Result<usize> {
        self.inner.purge_memories(ids).await
    }
    async fn find_near_duplicate(
        &self,
        content: &str,
        filter: MemoryFilter,
        min_similarity: f32,
    ) -> Result<Option<MemorySearchResult>> {
        self.inner
            .find_near_duplicate(content, filter, min_similarity)
            .await
    }
    async fn record_recurrence(&self, id: &MemoryId, more: Vec<String>) -> Result<u32> {
        self.inner.record_recurrence(id, more).await
    }
}

/// A sink keeping each comparison.
#[derive(Default)]
struct Kept(Mutex<Vec<Comparison>>);

impl ShadowSink for Kept {
    fn record(&self, comparison: Comparison) {
        self.0.lock().unwrap().push(comparison);
    }
}

impl Kept {
    async fn first(&self) -> Comparison {
        for _ in 0..400 {
            if let Some(c) = self.0.lock().unwrap().first() {
                return c.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("nothing compared");
    }
}

struct NoReplica;

#[async_trait]
impl Erases for NoReplica {
    async fn purge_identity(&self, _: &AgentId, _: &str) -> Result<usize> {
        Ok(0)
    }
    async fn purge_memories(&self, _: &[MemoryId]) -> Result<usize> {
        Ok(0)
    }
}

struct Free;
struct Let;

#[async_trait]
impl Held for Let {
    async fn release(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ErasureLock for Free {
    async fn hold(&self) -> Result<Box<dyn Held>> {
        Ok(Box::new(Let))
    }
}

struct Rig {
    store: ShadowedMemory,
    serving: Arc<Vectored>,
    replica: Arc<Vectored>,
    embedder: Arc<Counting>,
    kept: Arc<Kept>,
    shadow: Arc<Shadow>,
}

/// Weaviate serving `serving_model`'s vectors, vellev's replica the hub's,
/// each holding one memory about otters; recalls shadowed at every read.
async fn rig(serving_model: &'static str) -> Rig {
    let embedder = Arc::new(Counting::default());
    let serving = Arc::new(Vectored::new(serving_model, Arc::clone(&embedder)));
    let replica = Arc::new(Vectored::new("hub", Arc::clone(&embedder)));
    for store in [&serving, &replica] {
        store
            .inner
            .remember(
                Memory::new("otters hold hands", MemoryLayer::Episodic, agent())
                    .with_identity_id("bob"),
            )
            .await
            .unwrap();
    }
    let kept = Arc::new(Kept::default());
    let settings = WorkforceConfig::default()
        .vellev
        .shadow
        .settings(workforce_common::settings::ShadowedStore::Memories)
        .unwrap();
    let shadow = Arc::new(
        Shadow::new(
            "memories",
            settings,
            Arc::clone(&kept) as Arc<dyn ShadowSink>,
        )
        .unwrap(),
    );
    let store = ShadowedMemory::new(
        Arc::clone(&serving) as Arc<dyn MemoryStore>,
        Arc::new(NoReplica),
        Arc::new(Free),
    )
    .reading(Reads {
        embedder: Arc::clone(&embedder) as Arc<dyn EmbeddingBackend>,
        replica: Arc::clone(&replica) as Arc<dyn MemoryStore>,
        shadow: Arc::clone(&shadow),
    });
    Rig {
        store,
        serving,
        replica,
        embedder,
        kept,
        shadow,
    }
}

fn bob() -> MemoryFilter {
    MemoryFilter {
        identity_id: Some("bob".into()),
        ..MemoryFilter::default()
    }
}

/// Test 1. A replica that never answers, or fails, neither slows nor
/// changes the user's recall: it is Weaviate's answer, given at once.
#[tokio::test]
async fn a_hung_or_failing_replica_never_slows_or_changes_a_recall() {
    for answers in [Answers::Never, Answers::Failing] {
        let rig = rig("hub").await;
        *rig.replica.answers.lock().unwrap() = answers;
        let hits = rig
            .store
            .recall("otters", bob())
            .now_or_never()
            .expect("answered at once")
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory.content, "otters hold hands");
    }
}

/// Test 2. One embedding per recall, shadowed or not, and that one vector
/// is what both stores were given: the indexes are compared, not two
/// embedding calls (A5).
#[tokio::test]
async fn a_shadowed_recall_is_embedded_once_and_both_stores_get_that_vector() {
    let rig = rig("hub").await;
    rig.store.recall("otters", bob()).await.unwrap();
    let compared = rig.kept.first().await;
    assert!(
        matches!(compared.outcome, Outcome::Answered { .. }),
        "{:?}",
        compared.outcome
    );
    assert_eq!(rig.embedder.queries.load(Ordering::SeqCst), 1);
    let given = rig.serving.given.lock().unwrap().clone();
    assert_eq!(given, vec![Some(vec![6.0, 1.0])]);
    assert_eq!(rig.replica.given.lock().unwrap().clone(), given);
    assert!(compared
        .primary
        .labels
        .contains(&("vector", "given".to_string())));
}

/// Test 4. While Weaviate serves another model's vectors (memories still
/// being moved to the hub's), no vector is handed across models: the
/// recall is Weaviate's own, and not shadowed.
#[tokio::test]
async fn no_vector_is_handed_across_models() {
    let rig = rig("v1").await;
    let hits = rig.store.recall("otters", bob()).await.unwrap();
    assert_eq!(hits.len(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rig.serving.given.lock().unwrap().is_empty(),
        "embedded itself"
    );
    assert!(
        rig.replica.as_shadow.lock().unwrap().is_empty(),
        "not asked"
    );
    assert_eq!(rig.shadow.counts().offered, 0);
    assert_eq!(rig.embedder.queries.load(Ordering::SeqCst), 1);
}

/// Q2. When the query cannot be embedded, both are asked by keywords
/// alone, as Weaviate is then, and the comparison says so.
#[tokio::test]
async fn a_query_not_embedded_is_asked_of_both_by_keywords() {
    let rig = rig("hub").await;
    rig.embedder.down.store(true, Ordering::SeqCst);
    rig.store.recall("otters", bob()).await.unwrap();
    let compared = rig.kept.first().await;
    assert_eq!(rig.serving.given.lock().unwrap().clone(), vec![None]);
    assert_eq!(rig.replica.given.lock().unwrap().clone(), vec![None]);
    assert!(compared
        .primary
        .labels
        .contains(&("vector", "none".to_string())));
    assert_eq!(rig.embedder.queries.load(Ordering::SeqCst), 1);
}

/// Test 5 (A8). The replica's read is the shadow's: what it audits is
/// labelled so; the user's read is not.
#[tokio::test]
async fn the_replica_is_asked_as_the_shadow_and_the_user_s_read_is_not() {
    let rig = rig("hub").await;
    rig.store.recall("otters", bob()).await.unwrap();
    rig.kept.first().await;
    assert_eq!(rig.serving.as_shadow.lock().unwrap().clone(), vec![false]);
    assert_eq!(rig.replica.as_shadow.lock().unwrap().clone(), vec![true]);
}

/// Test 7. A recall is compared by memories, each once in the order
/// given, not by the objects holding them: Weaviate answering with one
/// memory held in two objects, and the replica with it in one, agree.
#[tokio::test]
async fn memories_are_compared_each_once() {
    let embedder = Arc::new(Counting::default());
    let mut serving = Vectored::new("hub", Arc::clone(&embedder));
    serving.twice = true;
    let serving = Arc::new(serving);
    let replica = Arc::new(Vectored::new("hub", Arc::clone(&embedder)));
    let memory = Memory::new("otters", MemoryLayer::Episodic, agent()).with_identity_id("bob");
    let id = serving.inner.remember(memory.clone()).await.unwrap();
    assert_eq!(replica.inner.remember(memory).await.unwrap(), id);
    let kept = Arc::new(Kept::default());
    let settings = WorkforceConfig::default()
        .vellev
        .shadow
        .settings(workforce_common::settings::ShadowedStore::Memories)
        .unwrap();
    let store = ShadowedMemory::new(
        Arc::clone(&serving) as Arc<dyn MemoryStore>,
        Arc::new(NoReplica),
        Arc::new(Free),
    )
    .reading(Reads {
        embedder: Arc::clone(&embedder) as Arc<dyn EmbeddingBackend>,
        replica: Arc::clone(&replica) as Arc<dyn MemoryStore>,
        shadow: Arc::new(
            Shadow::new(
                "memories",
                settings,
                Arc::clone(&kept) as Arc<dyn ShadowSink>,
            )
            .unwrap(),
        ),
    });

    let hits = store.recall("otters", bob()).await.unwrap();
    assert_eq!(hits.len(), 2, "the user's answer is Weaviate's, unchanged");
    let compared = kept.first().await;
    assert_eq!(compared.primary.ranked.ids, vec![id.0.clone()]);
    let Outcome::Answered { ranked, metrics } = compared.outcome else {
        panic!("answered: {:?}", compared.outcome);
    };
    assert_eq!(ranked.ids, vec![id.0]);
    assert!(metrics.top1);
    assert!((metrics.overlap - 1.0).abs() < f64::EPSILON, "{metrics:?}");
}

/// Test 6. Paused, the replica is asked nothing; the recall is answered
/// as ever.
#[tokio::test]
async fn paused_the_replica_is_asked_nothing() {
    let rig = rig("hub").await;
    rig.shadow.set_enabled(false);
    assert_eq!(rig.store.recall("otters", bob()).await.unwrap().len(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rig.replica.as_shadow.lock().unwrap().is_empty());
    assert_eq!(rig.shadow.counts().paused, 1);
}

/// A store with no read shadow recalls as Weaviate does.
#[tokio::test]
async fn without_reads_a_recall_is_weaviate_s_alone() {
    let embedder = Arc::new(Counting::default());
    let serving = Arc::new(Vectored::new("hub", Arc::clone(&embedder)));
    let store = ShadowedMemory::new(
        Arc::clone(&serving) as Arc<dyn MemoryStore>,
        Arc::new(NoReplica),
        Arc::new(Free),
    );
    store.recall("otters", bob()).await.unwrap();
    assert!(serving.given.lock().unwrap().is_empty());
    assert_eq!(embedder.queries.load(Ordering::SeqCst), 1);
}

/// The store in use while memories are moved to the hub's model hands a
/// given vector, and the model it takes, on to the store it holds.
#[tokio::test]
async fn while_memories_are_moved_the_store_in_use_takes_the_vector() {
    let embedder = Arc::new(Counting::default());
    let held = Arc::new(Vectored::new("hub", Arc::clone(&embedder)));
    let switching = crate::embeddings::Switching(parking_lot::RwLock::new(
        Arc::clone(&held) as Arc<dyn MemoryStore>
    ));
    assert_eq!(switching.query_model().as_deref(), Some("hub"));
    switching
        .recall_with_vector("otters", Some(vec![1.0, 2.0]), bob())
        .await
        .unwrap();
    assert_eq!(
        held.given.lock().unwrap().clone(),
        vec![Some(vec![1.0, 2.0])]
    );
    assert_eq!(embedder.queries.load(Ordering::SeqCst), 0);
}

/// The hub's kernel for memories is `[vellev.shadow.memories]`'s, not
/// another store's, and is paused as `[vellev.shadow] paused` says.
#[tokio::test]
async fn the_memories_kernel_is_configured_by_its_own_table_and_paused_as_said() {
    let recalled = |toml: &'static str| async move {
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let kept = Arc::new(Kept::default());
        let kernel = Arc::new(
            memory_shadow_kernel(&config, Arc::clone(&kept) as Arc<dyn ShadowSink>).unwrap(),
        );
        let embedder = Arc::new(Counting::default());
        let serving = Arc::new(Vectored::new("hub", Arc::clone(&embedder)));
        let replica = Arc::new(Vectored::new("hub", Arc::clone(&embedder)));
        let store = ShadowedMemory::new(
            serving as Arc<dyn MemoryStore>,
            Arc::new(NoReplica),
            Arc::new(Free),
        )
        .reading(Reads {
            embedder: embedder as Arc<dyn EmbeddingBackend>,
            replica: replica as Arc<dyn MemoryStore>,
            shadow: Arc::clone(&kernel),
        });
        store.recall("otters", bob()).await.unwrap();
        kernel.counts()
    };
    let paused = recalled("[vellev.shadow]\npaused = true\n").await;
    assert_eq!((paused.paused, paused.started), (1, 0));
    let unsampled = recalled("[vellev.shadow.memories]\nsample = 0.0\n").await;
    assert_eq!((unsampled.unsampled, unsampled.started), (1, 0));
    let others = recalled("[vellev.shadow.artifacts]\nsample = 0.0\n").await;
    assert_eq!((others.unsampled, others.started), (0, 1));
}

/// The read shadow is made without waiting for its sink to open: a hub
/// whose Postgres is slow or down at boot still opens memories, and each
/// comparison made meanwhile is dropped and counted, never waited for.
#[tokio::test]
async fn the_read_shadow_is_made_without_waiting_for_postgres() {
    let mut config = WorkforceConfig::default();
    config.embeddings.provider = workforce_common::settings::EmbeddingsProvider::Tei;
    config.embeddings.url = Some("http://127.0.0.1:9".into());
    let replica = Arc::new(Vectored::new("hub", Arc::new(Counting::default())));
    let never = std::future::pending::<Result<Arc<dyn ShadowSink>>>();
    let reads = tokio::time::timeout(
        Duration::from_secs(5),
        made_reads(&config, replica as Arc<dyn MemoryStore>, never),
    )
    .await
    .expect("made without waiting for the sink")
    .unwrap();
    assert_eq!(reads.shadow.counts().offered, 0);

    let deferred = Deferred::opening(std::future::pending(), Duration::from_secs(60));
    deferred.record(Comparison {
        store: "memories",
        primary: Observed {
            ranked: ranked(&[], None),
            latency: Duration::ZERO,
            query_hash: None,
            labels: Vec::new(),
        },
        shadow_latency: Duration::ZERO,
        queued: Duration::ZERO,
        index: None,
        concurrent: 1,
        outcome: Outcome::Failed("unopened".into()),
        probe: false,
    });
    assert_eq!(deferred.dropped(), 1);
}
