//! The known-item runs' rules (DW-676, PR 10), each written before it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::Duration as Span;
use serde_json::json;
use workforce_common::shadow::{MemorySink, ShadowCounts, ShadowSettings};
use workforce_core::types::AgentId;
use workforce_memory::admin::testing::Held;
use workforce_memory::store::{MemoryStoreCacheStats, RecallSettings};
use workforce_memory::types::{Memory, MemoryId, MemoryLayer, Visibility};

use super::*;

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
}

fn now() -> DateTime<Utc> {
    day(22).and_hms_opt(3, 5, 0).unwrap().and_utc()
}

/// An exported object of memory `memory`, filed under `identity`, written
/// `days_ago`.
fn object(id: &str, memory: &str, content: &str, identity: Option<&str>, days_ago: i64) -> Value {
    json!({
        "id": id,
        "properties": {
            "memoryId": memory,
            "content": content,
            "owner": "agent",
            "identityId": identity,
            "deleted": false,
            "tags": [],
            "createdAt": (now() - Span::days(days_ago)).to_rfc3339(),
        }
    })
}

fn words(memory: usize, n: usize) -> String {
    (0..n)
        .map(|w| format!("w{memory}x{w}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `n` objects, one a memory, each of 12 words, filed under one of three
/// identities, written a week ago.
fn objects(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            object(
                &format!("{i:08x}-0000-0000-0000-000000000000"),
                &format!("mem-{i}"),
                &words(i, 12),
                Some(["ann", "bob", "cy"][i % 3]),
                7,
            )
        })
        .collect()
}

fn drawn(seed: u64, items: usize, objects: &[Value]) -> (Vec<Item>, KnownItemsDrawn) {
    let mut draw = Draw::new(seed, items, items, now() - Span::minutes(30));
    for object in objects {
        draw.offer(object);
    }
    draw.finish()
}

/// The same seed and the same memories give the same items and the same
/// queries, whatever order the memories are read in; another day's seed
/// gives another set. A memory held by two objects is drawn once, with
/// the same query either way round.
#[test]
fn the_same_seed_draws_the_same_set_whatever_the_order() {
    let all = objects(300);
    let seed = run_seed(7, day(20));
    assert_eq!(seed, run_seed(7, day(20)), "a run's seed is not stable");
    assert_ne!(seed, run_seed(7, day(21)), "two days share a seed");
    assert_ne!(seed, run_seed(8, day(20)), "two base seeds share a seed");

    let (items, counted) = drawn(seed, 50, &all);
    assert_eq!(items.len(), 50);
    assert_eq!((counted.population, counted.drawn), (300, 50));
    let mut reversed = all.clone();
    reversed.reverse();
    assert_eq!(drawn(seed, 50, &reversed).0, items, "reversed, another set");
    let mut shuffled = all.clone();
    shuffled.sort_by_key(|o| {
        blake3::hash(o["id"].as_str().unwrap().as_bytes())
            .to_hex()
            .to_string()
    });
    assert_eq!(drawn(seed, 50, &shuffled).0, items, "shuffled, another set");

    let (other_day, _) = drawn(run_seed(7, day(21)), 50, &all);
    let ids = |items: &[Item]| -> BTreeSet<String> {
        items.iter().map(|i| i.memory_id.clone()).collect()
    };
    assert_ne!(
        ids(&other_day),
        ids(&items),
        "another day drew the same set"
    );
    for item in &items {
        assert!(item.identity_id.as_ref().is_some_and(|i| !i.is_empty()));
    }

    // One memory, two objects (a copy): drawn once, the same way round.
    let twice = vec![
        object(
            "0000000a-0000-0000-0000-000000000000",
            "m",
            &words(1, 12),
            Some("ann"),
            7,
        ),
        object(
            "0000000b-0000-0000-0000-000000000000",
            "m",
            &words(2, 12),
            Some("ann"),
            7,
        ),
    ];
    let (one, _) = drawn(seed, 10, &twice);
    let (other_way, _) = drawn(seed, 10, &[twice[1].clone(), twice[0].clone()]);
    assert_eq!(one.len(), 1, "{one:?}");
    assert_eq!(one, other_way);
}

/// What a recall scoped to the memory's identity cannot return is never
/// drawn, nor what the replica may not hold yet; each is counted by why.
#[test]
fn what_a_scoped_recall_cannot_return_is_never_drawn_and_is_counted() {
    let ok = object(
        "00000001-0000-0000-0000-000000000000",
        "ok",
        &words(1, 12),
        Some("ann"),
        7,
    );
    let mut deleted = object(
        "00000002-0000-0000-0000-000000000000",
        "deleted",
        &words(2, 12),
        Some("ann"),
        7,
    );
    deleted["properties"]["deleted"] = json!(true);
    let mut polluted = object(
        "00000003-0000-0000-0000-000000000000",
        "polluted",
        &words(3, 12),
        Some("ann"),
        7,
    );
    polluted["properties"]["tags"] = json!(["topic", workforce_memory::audit::RECAP_ARTIFACT_KIND]);
    let no_identity = object(
        "00000004-0000-0000-0000-000000000000",
        "none",
        &words(4, 12),
        None,
        7,
    );
    let empty_identity = object(
        "00000005-0000-0000-0000-000000000000",
        "empty",
        &words(5, 12),
        Some(""),
        7,
    );
    let recent = object(
        "00000006-0000-0000-0000-000000000000",
        "recent",
        &words(6, 12),
        Some("ann"),
        0,
    );
    let short = object(
        "00000007-0000-0000-0000-000000000000",
        "short",
        &words(7, MIN_WORDS - 1),
        Some("ann"),
        7,
    );
    let marks = object(
        "00000008-0000-0000-0000-000000000000",
        "marks",
        &["--"; 12].join(" "),
        Some("ann"),
        7,
    );
    let all = vec![
        ok,
        deleted,
        polluted,
        no_identity,
        empty_identity,
        recent,
        short,
        marks,
    ];
    let (items, counted) = drawn(run_seed(0, day(20)), 100, &all);
    let drawn: BTreeSet<(&str, Option<&str>)> = items
        .iter()
        .map(|i| (i.memory_id.as_str(), i.identity_id.as_deref()))
        .collect();
    // Those filed under no identity are drawn, as the unscoped population.
    assert_eq!(
        drawn,
        BTreeSet::from([("ok", Some("ann")), ("none", None), ("empty", None)])
    );
    assert_eq!(counted.population, 8);
    assert_eq!((counted.drawn, counted.drawn_unscoped), (3, 2));
    assert_eq!(
        counted.excluded,
        BTreeMap::from([
            ("deleted", 1),
            ("not_vectorizable", 1),
            ("polluted", 1),
            ("too_recent", 1),
            ("too_short", 1),
        ])
    );
}

/// A query is made from its memory, never copied whole: a run of its words,
/// in order, from 6 to 24 of them and fewer than it has, placed by the
/// seed. A memory too short for one makes none.
#[test]
fn a_query_is_a_window_of_its_memory_never_the_whole() {
    let mut placed = BTreeSet::new();
    for n in MIN_WORDS..60 {
        let content = words(n, n);
        for seed in 0..40u64 {
            let query = query_of(seed, "m", &content).expect("long enough");
            assert_eq!(
                query_of(seed, "m", &content).as_deref(),
                Some(query.as_str())
            );
            let taken = query.split_whitespace().count();
            assert!(
                (QUERY_WORDS.0..=QUERY_WORDS.1).contains(&taken) && taken < n,
                "{n} words gave a query of {taken}"
            );
            assert_ne!(query, content);
            assert!(content.contains(&query), "not a run of its words: {query}");
            placed.insert((n, query));
        }
    }
    assert!(
        placed.len() > 52 * 2,
        "the seed does not move where the window falls"
    );
    assert_eq!(query_of(1, "m", &words(1, MIN_WORDS - 1)), None);
    // Spaces of any kind between words are one.
    let spaced = "a  b\tc\nd e f g h i j";
    let query = query_of(3, "m", spaced).unwrap();
    assert!(!query.contains("  ") && !query.contains('\t') && !query.contains('\n'));
}

/// A run is due within its window, on the leader, while not paused; a
/// hub that comes up past the window waits for the next day's.
#[test]
fn a_run_is_due_within_its_window_on_the_leader_while_not_paused() {
    let at = |h: u32| day(22).and_hms_opt(h, 0, 0).unwrap().and_utc();
    assert!(!due(at(1), 2, 6, true, false), "before its window");
    assert!(due(at(2), 2, 6, true, false));
    assert!(due(at(5), 2, 6, true, false));
    assert!(!due(at(6), 2, 6, true, false), "past its window");
    assert!(!due(at(23), 2, 6, true, false), "a hub started late waits");
    assert!(due(at(23), 22, 24, true, false), "a window to midnight");
    assert!(!due(at(4), 2, 6, false, false), "off the leader");
    assert!(!due(at(4), 2, 6, true, true), "while paused");
}

// The runner, against stores that answer as told.

/// One recall asked of a store.
#[derive(Debug, Clone)]
struct Asked {
    query: String,
    vector: Option<Vec<f32>>,
    identity: Option<String>,
    alpha: Option<f32>,
    limit: Option<usize>,
    owner: Option<AgentId>,
    unscoped: bool,
    labelled_shadow: bool,
    /// Whether another of the kernel's permits could be had meanwhile.
    spare_permit: Option<bool>,
}

type Answer = Box<dyn Fn(&str, &Asked) -> Result<Vec<String>> + Send + Sync>;

/// A store that answers each recall with the memory ids `answer` gives,
/// keeping what it was asked.
struct Answers {
    model: &'static str,
    alpha: f32,
    answer: Answer,
    asked: Mutex<Vec<Asked>>,
    kernel: Mutex<Option<Arc<Shadow>>>,
    /// The content each memory found holds, by id; "found" otherwise.
    contents: Mutex<BTreeMap<String, String>>,
}

impl Answers {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            model: "hub",
            alpha: 0.75,
            answer,
            asked: Mutex::new(Vec::new()),
            kernel: Mutex::new(None),
            contents: Mutex::new(BTreeMap::new()),
        })
    }

    /// Itself first, then `other`.
    fn itself_first() -> Arc<Self> {
        Self::new(Box::new(|memory, _| {
            Ok(vec![memory.to_string(), "other".to_string()])
        }))
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }
}

fn found(id: &str, content: Option<&String>) -> MemorySearchResult {
    let content = content.map_or("found", String::as_str);
    let mut memory = Memory::new(content, MemoryLayer::Episodic, AgentId::new("agent"));
    memory.id = MemoryId(id.to_string());
    MemorySearchResult { memory, score: 0.5 }
}

#[async_trait]
impl MemoryStore for Answers {
    fn cache_stats(&self) -> MemoryStoreCacheStats {
        MemoryStoreCacheStats::default()
    }
    fn query_model(&self) -> Option<String> {
        Some(self.model.to_string())
    }
    fn recall_settings(&self) -> Option<RecallSettings> {
        Some(RecallSettings {
            alpha: self.alpha,
            reranked: false,
            rerank_candidates: 0,
            score_map: Vec::new(),
        })
    }
    async fn recall_with_vector(
        &self,
        query: &str,
        vector: Option<Vec<f32>>,
        filter: MemoryFilter,
    ) -> Result<Vec<MemorySearchResult>> {
        let kernel = self.kernel.lock().unwrap().clone();
        let spare_permit = match kernel {
            Some(kernel) => Some(
                tokio::time::timeout(Duration::from_millis(50), kernel.permit())
                    .await
                    .is_ok_and(|p| p.is_some()),
            ),
            None => None,
        };
        let asked = Asked {
            query: query.to_string(),
            vector,
            identity: filter.identity_id.clone(),
            alpha: filter.hybrid_alpha,
            limit: filter.limit,
            owner: filter.owner.clone(),
            unscoped: filter.system_unscoped,
            labelled_shadow: workforce_memory::access_audit::in_shadow(),
            spare_permit,
        };
        self.asked.lock().unwrap().push(asked.clone());
        // The memory a query was made from is the one whose words it holds.
        let memory = query
            .split_whitespace()
            .next()
            .and_then(|w| w.strip_prefix('w'))
            .and_then(|w| w.split('x').next())
            .map(|n| format!("mem-{n}"))
            .unwrap_or_default();
        let ids = (self.answer)(&memory, &asked)?;
        let contents = self.contents.lock().unwrap();
        Ok(ids.iter().map(|id| found(id, contents.get(id))).collect())
    }
    async fn remember(&self, _: Memory) -> Result<MemoryId> {
        unreachable!("a known-item run never writes")
    }
    async fn recall(&self, _: &str, _: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        unreachable!("a known-item run hands its vector")
    }
    async fn get(&self, _: &MemoryId) -> Result<Option<Memory>> {
        unreachable!()
    }
    async fn forget(&self, _: &MemoryId) -> Result<()> {
        unreachable!("a known-item run never writes")
    }
    async fn share(&self, _: &MemoryId, _: Visibility) -> Result<()> {
        unreachable!("a known-item run never writes")
    }
    async fn count(&self, _: MemoryFilter) -> Result<usize> {
        unreachable!()
    }
    async fn list(&self, _: MemoryFilter) -> Result<Vec<Memory>> {
        unreachable!()
    }
    async fn update_tags(&self, _: &MemoryId, _: Vec<String>) -> Result<()> {
        unreachable!("a known-item run never writes")
    }
    async fn adjust_importance(&self, _: &MemoryId, _: f32) -> Result<f32> {
        unreachable!("a known-item run never writes")
    }
    async fn identity_memories(&self, _: &AgentId, _: &str) -> Result<Vec<MemoryId>> {
        unreachable!()
    }
    async fn identity_counts(&self, _: &AgentId) -> Result<BTreeMap<String, usize>> {
        unreachable!()
    }
    async fn reassign_identity(&self, _: &AgentId, _: &str, _: &str) -> Result<usize> {
        unreachable!("a known-item run never writes")
    }
    async fn purge_identity(&self, _: &AgentId, _: &str) -> Result<usize> {
        unreachable!("a known-item run never writes")
    }
    async fn purge_memories(&self, _: &[MemoryId]) -> Result<usize> {
        unreachable!("a known-item run never writes")
    }
    async fn record_recurrence(&self, _: &MemoryId, _: Vec<String>) -> Result<u32> {
        unreachable!("a known-item run never writes")
    }
}

/// Embeds as the hub's model, counting, failing for a query of memory 13.
#[derive(Default)]
struct Hub {
    embedded: Mutex<Vec<String>>,
}

#[async_trait]
impl EmbeddingBackend for Hub {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embedded.lock().unwrap().push(text.to_string());
        if text.starts_with("w13x") {
            return Err(eyre!("the embedder answered 503"));
        }
        Ok(vec![text.len() as f32, 1.0])
    }

    fn model_id(&self) -> &str {
        "hub"
    }
}

/// Keeps what a run records; the day claimed unless told otherwise.
#[derive(Default)]
struct Kept {
    taken: AtomicBool,
    claims: Mutex<Vec<KnownItemClaim>>,
    drawn: Mutex<Option<KnownItemsDrawn>>,
    items: Mutex<Vec<KnownItemRow>>,
    finished: Mutex<Option<(KnownItemOutcome, Option<String>)>>,
}

#[async_trait]
impl Recorder for Kept {
    async fn claim(&self, claim: &KnownItemClaim) -> Result<Option<i64>> {
        self.claims.lock().unwrap().push(claim.clone());
        Ok((!self.taken.load(Ordering::SeqCst)).then_some(1))
    }
    async fn drawn(&self, _: i64, drawn: &KnownItemsDrawn) -> Result<()> {
        *self.drawn.lock().unwrap() = Some(drawn.clone());
        Ok(())
    }
    async fn items(&self, _: i64, items: &[KnownItemRow]) -> Result<()> {
        self.items.lock().unwrap().extend_from_slice(items);
        Ok(())
    }
    async fn finish(&self, _: i64, outcome: KnownItemOutcome, error: Option<&str>) -> Result<bool> {
        *self.finished.lock().unwrap() = Some((outcome, error.map(String::from)));
        Ok(true)
    }
}

struct Rig {
    serving: Arc<Answers>,
    replica: Arc<Answers>,
    hub: Arc<Hub>,
    kernel: Arc<Shadow>,
    sink: Arc<MemorySink>,
    kept: Arc<Kept>,
    runner: Runner,
}

fn rig(n: usize, permits: usize, serving: Arc<Answers>, replica: Arc<Answers>) -> Rig {
    let sink = Arc::new(MemorySink::default());
    let kernel = Arc::new(
        Shadow::new(
            "memories",
            ShadowSettings {
                sample: 1.0,
                permits,
                deadline: Duration::from_secs(2),
                trip_after: 3,
                rest: Duration::from_secs(30),
                k: 10,
                rbo_p: 0.9,
            },
            sink.clone(),
        )
        .unwrap(),
    );
    let hub = Arc::new(Hub::default());
    let kept = Arc::new(Kept::default());
    let runner = Runner {
        source: Arc::new(Held::holding(Some("hub"), objects(n))),
        serving: serving.clone(),
        replica: replica.clone(),
        embedder: hub.clone(),
        kernel: Arc::clone(&kernel),
        recorder: kept.clone(),
        settings: KnownItemSettings {
            items: n,
            unscoped_items: n,
            seed: 7,
            hour_utc: 2,
            until_hour_utc: 6,
            pace: Duration::ZERO,
        },
        k: 10,
        rbo_p: 0.9,
        settle: Duration::from_secs(30 * 60),
        permit_wait: Duration::from_secs(5),
        page_pace: Duration::ZERO,
    };
    Rig {
        serving,
        replica,
        hub,
        kernel,
        sink,
        kept,
        runner,
    }
}

fn always() -> bool {
    true
}

/// Each item is embedded once, and that vector handed to both stores at
/// both alphas, the serving store's own and 1.0, scoped to the memory's
/// identity alone, read to depth k, labelled a shadow read; where each
/// side put the memory is scored by memory id.
#[tokio::test]
async fn an_item_is_embedded_once_and_asked_of_both_stores_at_both_alphas() {
    let replica = Answers::new(Box::new(|memory, _| {
        Ok(vec!["other".to_string(), memory.to_string()])
    }));
    let r = rig(12, 4, Answers::itself_first(), replica);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(
        (ran.outcome, ran.items, ran.drawn.drawn),
        (KnownItemOutcome::Ok, 12, 12)
    );
    assert_eq!(
        r.hub.embedded.lock().unwrap().len(),
        24,
        "embedded once an item"
    );
    let claim = r.kept.claims.lock().unwrap()[0].clone();
    assert_eq!(
        (
            claim.store.as_str(),
            claim.run_day,
            claim.base_seed,
            claim.derivation,
            claim.k
        ),
        (STORE, day(22), 7, DERIVATION, 10)
    );
    assert_eq!(
        (claim.alpha_serving, claim.run_seed),
        (0.75, run_seed(7, day(22)))
    );

    let mut asked: BTreeMap<String, Vec<(&str, Asked)>> = BTreeMap::new();
    for (side, store) in [("serving", &r.serving), ("replica", &r.replica)] {
        for a in store.asked() {
            asked.entry(a.query.clone()).or_default().push((side, a));
        }
    }
    assert_eq!(asked.len(), 24);
    for (query, calls) in &asked {
        assert_eq!(calls.len(), 4, "{query}: {calls:?}");
        let vector = calls[0].1.vector.clone().expect("a vector handed");
        assert_eq!(
            vector,
            vec![query.len() as f32, 1.0],
            "not the query's own vector"
        );
        let mut alphas: Vec<(&str, f32)> =
            calls.iter().map(|(s, a)| (*s, a.alpha.unwrap())).collect();
        alphas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            alphas,
            vec![
                ("replica", 0.75),
                ("replica", 1.0),
                ("serving", 0.75),
                ("serving", 1.0)
            ]
        );
        for (_, a) in calls {
            assert_eq!(
                a.vector.as_ref(),
                Some(&vector),
                "the vector differs between calls"
            );
            assert!(a.identity.as_deref().is_some_and(|i| !i.is_empty()));
            assert_eq!(
                (a.limit, a.owner.is_none(), a.unscoped),
                (Some(10), true, false)
            );
            assert!(
                a.labelled_shadow,
                "a run's recall is not labelled a shadow read"
            );
        }
    }
    let items = r.kept.items.lock().unwrap().clone();
    assert_eq!(items.len(), 48, "two modes and two alphas an item");
    for item in &items {
        assert_eq!(item.status, KnownItemStatus::Asked);
        assert_eq!((item.primary.rank, item.shadow.rank), (Some(1), Some(2)));
        assert_eq!((item.primary.count, item.shadow.count), (Some(2), Some(2)));
        assert!(item.primary.ms.is_some() && item.shadow.ms.is_some());
        let metrics = item.metrics.expect("both answered");
        assert!(!metrics.top1 && metrics.overlap == 1.0);
    }
    assert_eq!(
        r.kept.finished.lock().unwrap().clone(),
        Some((KnownItemOutcome::Ok, None))
    );
}

/// A run asks nothing without one of the memory shadow's permits, waits
/// for one rather than dropping its item, and never holds more than one:
/// the live samples keep the rest.
#[tokio::test]
async fn a_run_waits_for_a_permit_of_the_shadows_and_holds_one_at_most() {
    let r = rig(5, 1, Answers::itself_first(), Answers::itself_first());
    let held = r.kernel.permit().await.unwrap();
    let runner = Arc::new(r.runner);
    let running = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.run(day(22), now(), &always).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        r.serving.asked().is_empty() && r.replica.asked().is_empty(),
        "asked while every permit was taken"
    );
    drop(held);
    let ran = running.await.unwrap().unwrap().unwrap();
    assert_eq!((ran.outcome, ran.items), (KnownItemOutcome::Ok, 5));

    let r = rig(5, 2, Answers::itself_first(), Answers::itself_first());
    *r.serving.kernel.lock().unwrap() = Some(Arc::clone(&r.kernel));
    *r.replica.kernel.lock().unwrap() = Some(Arc::clone(&r.kernel));
    r.runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    let spare: Vec<Option<bool>> = r
        .serving
        .asked()
        .into_iter()
        .chain(r.replica.asked())
        .map(|a| a.spare_permit)
        .collect();
    assert_eq!(spare.len(), 40);
    assert!(
        spare.iter().all(|s| *s == Some(true)),
        "a run held both permits of two: {spare:?}"
    );
}

/// Permits that stay taken cost an item its turn, recorded as such, and a
/// run whose items keep finding none ends part way rather than wait all
/// night.
#[tokio::test]
async fn permits_that_stay_taken_are_recorded_and_end_the_run() {
    let mut r = rig(
        VELLEV_DOWN_AFTER + 5,
        1,
        Answers::itself_first(),
        Answers::itself_first(),
    );
    r.runner.permit_wait = Duration::from_millis(20);
    let _held = r.kernel.permit().await.unwrap();
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Incomplete);
    let items = r.kept.items.lock().unwrap().clone();
    assert_eq!(items.len(), VELLEV_DOWN_AFTER * 4);
    assert!(items.iter().all(|i| i.status == KnownItemStatus::NoPermit));
    assert!(r.serving.asked().is_empty());
}

/// Paused before a run, none is claimed; paused part way, the run stops,
/// keeping the items asked.
#[tokio::test]
async fn a_pause_stops_a_run() {
    let r = rig(10, 4, Answers::itself_first(), Answers::itself_first());
    r.kernel.set_enabled(false);
    assert_eq!(r.runner.run(day(22), now(), &always).await.unwrap(), None);
    assert!(
        r.kept.claims.lock().unwrap().is_empty(),
        "claimed while paused"
    );
    assert!(r.serving.asked().is_empty());

    let r = rig(10, 4, Answers::itself_first(), Answers::itself_first());
    let looked = AtomicUsize::new(0);
    let go = || looked.fetch_add(1, Ordering::SeqCst) < 4;
    let ran = r.runner.run(day(22), now(), &go).await.unwrap().unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Incomplete);
    assert!(ran.error.is_some());
    assert_eq!(
        r.kept.items.lock().unwrap().len(),
        3 * 4,
        "the items asked are kept (the draw looked once)"
    );
    assert_eq!(r.serving.asked().len(), 3 * 4);
}

/// A run's recalls are not live samples: the shadow's record and counts
/// are as they were.
#[tokio::test]
async fn a_run_adds_nothing_to_the_live_shadows_record() {
    let r = rig(10, 4, Answers::itself_first(), Answers::itself_first());
    r.runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        r.sink.is_empty(),
        "a run's recall was recorded as a live sample"
    );
    assert_eq!(r.kernel.counts(), ShadowCounts::default());
}

/// A side that fails is recorded as failing, never as missing the memory;
/// a query that cannot be embedded is asked of neither side. vellev
/// failing item after item ends the run part way.
#[tokio::test]
async fn an_error_is_recorded_as_such_never_as_a_miss() {
    let replica = Answers::new(Box::new(|memory, _| {
        if memory == "mem-4" {
            return Err(eyre!("vellev answered 503"));
        }
        Ok(vec![memory.to_string()])
    }));
    let r = rig(20, 4, Answers::itself_first(), replica);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Ok);
    let items = r.kept.items.lock().unwrap().clone();
    let of = |memory: &str| -> Vec<KnownItemRow> {
        items
            .iter()
            .filter(|i| i.item_id == memory)
            .cloned()
            .collect()
    };
    for failed in of("mem-4") {
        assert_eq!(failed.status, KnownItemStatus::Asked);
        assert_eq!(failed.primary.rank, Some(1));
        assert_eq!(failed.shadow.rank, None);
        assert!(failed
            .shadow
            .error
            .as_deref()
            .is_some_and(|e| e.contains("503")));
        assert_eq!(failed.metrics, None);
    }
    let unembedded = of("mem-13");
    assert_eq!(unembedded.len(), 4);
    for item in &unembedded {
        assert_eq!(item.status, KnownItemStatus::EmbedFailed);
        assert_eq!((item.primary.rank, item.shadow.rank), (None, None));
    }
    assert!(
        r.serving
            .asked()
            .iter()
            .all(|a| !a.query.starts_with("w13x")),
        "a query that could not be embedded was asked"
    );

    let down = Answers::new(Box::new(|_, _| Err(eyre!("connection refused"))));
    let r = rig(VELLEV_DOWN_AFTER + 10, 4, Answers::itself_first(), down);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Incomplete);
    assert!(
        ran.error.as_deref().is_some_and(|e| e.contains("vellev")),
        "{ran:?}"
    );
    let asked: BTreeSet<String> = r
        .kept
        .items
        .lock()
        .unwrap()
        .iter()
        .filter(|i| i.status == KnownItemStatus::Asked)
        .map(|i| i.item_id.clone())
        .collect();
    assert_eq!(
        asked.len(),
        VELLEV_DOWN_AFTER,
        "vellev down, the run went on"
    );
}

/// Weaviate, which serves users, failing item after item ends the run part
/// way, as vellev does; and so does the embedder, whose calls may cost.
#[tokio::test]
async fn weaviate_or_the_embedder_failing_in_a_row_ends_the_run() {
    let down = Answers::new(Box::new(|_, _| Err(eyre!("connection refused"))));
    let r = rig(VELLEV_DOWN_AFTER + 10, 4, down, Answers::itself_first());
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Incomplete);
    assert!(
        ran.error.as_deref().is_some_and(|e| e.contains("Weaviate")),
        "{ran:?}"
    );
    // Two modes and two alphas an item; the one item whose query the rig
    // cannot embed is asked of neither.
    assert_eq!(
        r.serving.asked().len(),
        VELLEV_DOWN_AFTER * 4,
        "Weaviate down, the run went on"
    );

    struct Down;
    #[async_trait]
    impl EmbeddingBackend for Down {
        async fn embed(&self, _: &str) -> Result<Vec<f32>> {
            Err(eyre!("the embedder answered 503"))
        }
        fn model_id(&self) -> &str {
            "hub"
        }
    }
    let mut r = rig(
        VELLEV_DOWN_AFTER + 10,
        4,
        Answers::itself_first(),
        Answers::itself_first(),
    );
    r.runner.embedder = Arc::new(Down);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Incomplete);
    assert!(
        ran.error.as_deref().is_some_and(|e| e.contains("embedder")),
        "{ran:?}"
    );
    assert_eq!(
        ran.items, VELLEV_DOWN_AFTER,
        "the embedder down, the run went on"
    );
}

/// Only failures in a row end a run: vellev failing every other item, many
/// more items than the bound in all, leaves the run to its end.
#[tokio::test]
async fn failures_not_in_a_row_do_not_end_a_run() {
    let flaky = Answers::new(Box::new(|memory, _| {
        let n: usize = memory.trim_start_matches("mem-").parse().unwrap();
        if n.is_multiple_of(2) {
            return Err(eyre!("vellev answered 503"));
        }
        Ok(vec![memory.to_string()])
    }));
    let r = rig(VELLEV_DOWN_AFTER * 4, 4, Answers::itself_first(), flaky);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Ok, "{ran:?}");
    assert_eq!(ran.items, VELLEV_DOWN_AFTER * 4);
}

/// A day claimed already is not run: nothing is drawn or asked.
#[tokio::test]
async fn a_day_claimed_already_is_not_run() {
    let r = rig(10, 4, Answers::itself_first(), Answers::itself_first());
    r.kept.taken.store(true, Ordering::SeqCst);
    assert_eq!(r.runner.run(day(22), now(), &always).await.unwrap(), None);
    assert!(r.kept.drawn.lock().unwrap().is_none());
    assert!(r.serving.asked().is_empty());
    assert!(r.hub.embedded.lock().unwrap().is_empty());
}

/// Stores that do not both hold the hub's vectors are not compared: no
/// vector is handed across models, and nothing is claimed.
#[tokio::test]
async fn stores_not_of_the_hubs_model_are_not_run() {
    let other = Arc::new(Answers {
        model: "t2v",
        alpha: 0.75,
        answer: Box::new(|m, _| Ok(vec![m.to_string()])),
        asked: Mutex::new(Vec::new()),
        kernel: Mutex::new(None),
        contents: Mutex::new(BTreeMap::new()),
    });
    let r = rig(10, 4, other, Answers::itself_first());
    let refused = r.runner.run(day(22), now(), &always).await.unwrap_err();
    assert!(format!("{refused:#}").contains("t2v"), "{refused:#}");
    assert!(r.kept.claims.lock().unwrap().is_empty());
}

// Against a vellev node.

/// Embeds as a hub model named `hub`: each word hashed into one of 64
/// places, so that text sharing words lies near.
struct Words;

#[async_trait]
impl EmbeddingBackend for Words {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut vector = vec![0.0f32; 64];
        for word in text.split_whitespace() {
            vector[blake3::hash(word.to_lowercase().as_bytes()).as_bytes()[0] as usize % 64] += 1.0;
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
        Ok(vector.into_iter().map(|v| v / norm).collect())
    }

    fn model_id(&self) -> &str {
        "hub"
    }
}

/// vellev's replica of memories, for the hub's model `hub`, in `node`'s
/// tenant, holding `objects` with [`Words`]' vectors.
async fn node_replica(
    node: &workforce_vellev::testing::Node,
    objects: &[Value],
) -> Arc<dyn workforce_memory::admin::Switchable> {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.memory.backend = workforce_common::settings::SHADOWED.into();
    config.vellev.url = node.url.clone();
    config.vellev.api_key = Some(node.key.clone());
    config.vellev.tenant = node.tenant.clone();
    config.weaviate.enabled = true;
    let (replica, _) = crate::embeddings::paired(&config, Arc::new(Words), None).unwrap();
    replica.ensure_collection().await.unwrap();
    let mut held = Vec::new();
    for object in objects {
        let mut object = object.clone();
        let content = object["properties"]["content"]
            .as_str()
            .unwrap()
            .to_string();
        object["vector"] = json!(Words.embed(&content).await.unwrap());
        held.push(object);
    }
    assert_eq!(replica.upsert(&held).await.unwrap(), held.len());
    replica
}

/// vellev weighs a recall handed a vector by the alpha the recall names,
/// not its own: at 1.0 by the vector alone, at 0.0 by the words alone.
/// (A known-item run asks both alphas of the one store.)
#[tokio::test]
async fn vellev_recalls_at_the_alpha_each_recall_names() {
    let otters = "otters hold hands while they sleep in the river";
    let budget = "the quarterly budget review meeting moved to thursday afternoon";
    let objects = vec![
        object(
            "00000001-0000-4000-8000-000000000000",
            "otters",
            otters,
            Some("ann"),
            7,
        ),
        object(
            "00000002-0000-4000-8000-000000000000",
            "budget",
            budget,
            Some("ann"),
            7,
        ),
    ];
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    let replica = node_replica(&node, &objects).await;
    let vector = Words.embed(otters).await.unwrap();
    let first = |alpha: f32| {
        let replica = Arc::clone(&replica);
        let vector = vector.clone();
        async move {
            let hits = replica
                .recall_with_vector(
                    "quarterly budget review",
                    Some(vector),
                    MemoryFilter {
                        identity_id: Some("ann".into()),
                        limit: Some(10),
                        hybrid_alpha: Some(alpha),
                        ..MemoryFilter::default()
                    },
                )
                .await
                .unwrap();
            hits.first().map(|h| h.memory.id.0.clone())
        }
    };
    assert_eq!(
        first(1.0).await.as_deref(),
        Some("otters"),
        "at 1.0, not by the vector"
    );
    assert_eq!(
        first(0.0).await.as_deref(),
        Some("budget"),
        "at 0.0, not by the words"
    );
}

/// A run made against the node, vellev answering for both sides, finds
/// each memory drawn first at both alphas and records it so.
#[tokio::test]
async fn a_run_against_the_node_finds_each_memory_first() {
    let all = objects(40);
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    let replica = node_replica(&node, &all).await;
    let sink = Arc::new(MemorySink::default());
    let kernel = Arc::new(
        Shadow::new(
            "memories",
            ShadowSettings {
                sample: 1.0,
                permits: 4,
                deadline: Duration::from_secs(10),
                trip_after: 3,
                rest: Duration::from_secs(30),
                k: 10,
                rbo_p: 0.9,
            },
            sink.clone(),
        )
        .unwrap(),
    );
    let kept = Arc::new(Kept::default());
    let runner = Runner {
        source: Arc::new(Held::holding(Some("hub"), all)),
        serving: Arc::clone(&replica) as Arc<dyn MemoryStore>,
        replica: replica as Arc<dyn MemoryStore>,
        embedder: Arc::new(Words),
        kernel,
        recorder: kept.clone(),
        settings: KnownItemSettings {
            items: 25,
            unscoped_items: 0,
            seed: 1,
            hour_utc: 2,
            until_hour_utc: 6,
            pace: Duration::ZERO,
        },
        k: 10,
        rbo_p: 0.9,
        settle: Duration::from_secs(30 * 60),
        permit_wait: Duration::from_secs(5),
        page_pace: Duration::ZERO,
    };
    let ran = runner.run(day(22), now(), &always).await.unwrap().unwrap();
    assert_eq!(
        (ran.outcome, ran.items, ran.drawn.drawn),
        (KnownItemOutcome::Ok, 25, 25),
        "{ran:?}"
    );
    let items = kept.items.lock().unwrap().clone();
    assert_eq!(items.len(), 100, "two modes and two alphas an item");
    for item in &items {
        assert_eq!(item.status, KnownItemStatus::Asked);
        assert_eq!(
            (item.primary.rank, item.shadow.rank, &item.shadow.error),
            (Some(1), Some(1), &None),
            "{item:?}"
        );
    }
    assert!(sink.is_empty());
}

// Review of #440: duplicates, the unscoped population, the hard mode,
// recent edits, and stopping a draw.

/// A memory found only as another of the same content (S1), its words
/// spaced and cased otherwise, counts as found, at that rank, and is
/// marked as found by a duplicate; one of other content is no find.
#[tokio::test]
async fn a_memory_found_as_its_duplicate_counts_as_found_and_is_marked() {
    // Weaviate puts a duplicate first and the memory nowhere; vellev puts
    // something else first, then the memory itself.
    let serving = Answers::new(Box::new(|memory, _| Ok(vec![format!("dup-{memory}")])));
    let replica = Answers::new(Box::new(|memory, _| {
        Ok(vec!["other".to_string(), memory.to_string()])
    }));
    let r = rig(6, 4, serving, replica);
    for i in 0..6 {
        let shouted = words(i, 12).to_uppercase().replace(' ', "   ");
        r.serving
            .contents
            .lock()
            .unwrap()
            .insert(format!("dup-mem-{i}"), shouted);
    }
    r.replica
        .contents
        .lock()
        .unwrap()
        .insert("other".into(), words(99, 12));
    r.runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    let items = r.kept.items.lock().unwrap().clone();
    assert_eq!(items.len(), 24);
    for item in &items {
        assert_eq!(
            (item.primary.rank, item.primary.by_duplicate),
            (Some(1), true),
            "{item:?}"
        );
        assert_eq!(
            (item.shadow.rank, item.shadow.by_duplicate),
            (Some(2), false)
        );
    }
}

/// Memories filed under no identity are drawn as their own population, up
/// to their own number, and recalled unscoped, as system jobs recall them;
/// those under one never are.
#[tokio::test]
async fn memories_under_no_identity_are_recalled_unscoped_and_kept_apart() {
    let mut r = rig(4, 4, Answers::itself_first(), Answers::itself_first());
    let mut all = objects(4);
    for i in 4..10 {
        all.push(object(
            &format!("{i:08x}-0000-0000-0000-000000000000"),
            &format!("mem-{i}"),
            &words(i, 12),
            None,
            7,
        ));
    }
    r.runner.source = Arc::new(Held::holding(Some("hub"), all));
    r.runner.settings.unscoped_items = 3;
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (ran.drawn.drawn, ran.drawn.drawn_unscoped),
        (7, 3),
        "{ran:?}"
    );
    let items = r.kept.items.lock().unwrap().clone();
    let unscoped: BTreeSet<String> = items
        .iter()
        .filter(|i| i.population == Population::Unscoped)
        .map(|i| i.item_id.clone())
        .collect();
    assert_eq!(unscoped.len(), 3);
    assert!(unscoped
        .iter()
        .all(|m| m["mem-".len()..].parse::<usize>().unwrap() >= 4));
    for a in r.serving.asked().into_iter().chain(r.replica.asked()) {
        let n: usize = a.query[1..].split('x').next().unwrap().parse().unwrap();
        if n >= 4 {
            assert_eq!((a.identity.as_deref(), a.unscoped), (None, true), "{a:?}");
        } else {
            assert!(a.identity.is_some() && !a.unscoped, "{a:?}");
        }
    }
}

/// The hard mode's query (S3) is 4 to 8 words in order, from the middle
/// half of the memory's words, fewer than the window's; each memory is
/// asked in both modes.
#[tokio::test]
async fn each_memory_is_asked_in_both_modes_the_hard_one_short_and_from_the_middle() {
    for n in MIN_WORDS..80 {
        let content = words(n, n);
        let all: Vec<&str> = content.split_whitespace().collect();
        for seed in 0..30u64 {
            let hard = hard_query_of(seed, "m", &content).unwrap();
            let taken = hard.split_whitespace().count();
            assert!((4..=8).contains(&taken) && taken < n, "{n}: {taken}");
            assert!(content.contains(&hard));
            let start = all
                .iter()
                .position(|w| *w == hard.split_whitespace().next().unwrap())
                .unwrap();
            let spare = n - taken;
            assert!(
                start >= spare / 4 && start <= (3 * spare).div_ceil(4),
                "{n} words: started at {start}, not in the middle"
            );
        }
    }
    assert_eq!(hard_query_of(1, "m", &words(1, MIN_WORDS - 1)), None);

    let r = rig(3, 4, Answers::itself_first(), Answers::itself_first());
    r.runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    let items = r.kept.items.lock().unwrap().clone();
    for mode in [QueryMode::Window, QueryMode::Hard] {
        assert_eq!(
            items.iter().filter(|i| i.mode == mode).count(),
            6,
            "{mode:?}"
        );
    }
    // Each memory (12 words) was asked by its window (6 words) and by its
    // hard query (4), each at both alphas.
    let mut asked: BTreeMap<String, BTreeMap<usize, usize>> = BTreeMap::new();
    for a in r.serving.asked() {
        let memory = a.query.split('x').next().unwrap().to_string();
        *asked
            .entry(memory)
            .or_default()
            .entry(a.query.split_whitespace().count())
            .or_default() += 1;
    }
    assert_eq!(asked.len(), 3);
    for (memory, by_length) in asked {
        assert_eq!(by_length, BTreeMap::from([(4, 2), (6, 2)]), "{memory}");
    }
}

/// A memory written again lately (its recurrence, or an `updatedAt`) is
/// too recent to draw, as one made lately is (S4).
#[test]
fn a_memory_written_again_lately_is_too_recent() {
    let mut again = object(
        "00000001-0000-0000-0000-000000000000",
        "again",
        &words(1, 12),
        Some("ann"),
        7,
    );
    again["properties"]["lastRecurrenceAt"] = json!(now().to_rfc3339());
    let mut updated = object(
        "00000002-0000-0000-0000-000000000000",
        "updated",
        &words(2, 12),
        Some("ann"),
        7,
    );
    updated["properties"]["updatedAt"] = json!(now().to_rfc3339());
    let mut long_ago = object(
        "00000003-0000-0000-0000-000000000000",
        "old",
        &words(3, 12),
        Some("ann"),
        7,
    );
    long_ago["properties"]["lastRecurrenceAt"] = json!((now() - Span::days(3)).to_rfc3339());
    let (items, counted) = drawn(run_seed(0, day(20)), 10, &[again, updated, long_ago]);
    assert_eq!(
        items
            .iter()
            .map(|i| i.memory_id.as_str())
            .collect::<Vec<_>>(),
        ["old"]
    );
    assert_eq!(counted.excluded, BTreeMap::from([("too_recent", 2)]));
}

/// A run told to stop while it draws stops there, part way, asking
/// nothing.
#[tokio::test]
async fn a_run_stops_while_it_draws() {
    let r = rig(250, 4, Answers::itself_first(), Answers::itself_first());
    let looked = AtomicUsize::new(0);
    // On to the second page, then stop.
    let go = || looked.fetch_add(1, Ordering::SeqCst) < 1;
    let ran = r.runner.run(day(22), now(), &go).await.unwrap().unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Incomplete);
    assert!(
        ran.error.as_deref().is_some_and(|e| e.contains("drawing")),
        "{ran:?}"
    );
    assert!(r.serving.asked().is_empty() && r.kept.items.lock().unwrap().is_empty());
}
