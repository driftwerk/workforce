//! The rules of vellev's promotion (DW-676, PR 11), each pinned by a test
//! a plant must turn red: the start-up guards decide on their facts alone,
//! promotion writes nothing until every step passed and then writes
//! Postgres's record before vellev's marker, and a rollback carries back
//! everything written on vellev before it takes either away.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use workforce_common::shadow_postgres::{ArtifactMirrorFacts, MemoryShadowFacts, ReconcileRun};
use workforce_memory::admin::testing::Held;
use workforce_memory::admin::{NoTurns, SyncSource, SyncTarget};

use super::*;
use crate::memory_shadow::{Reconcile, Recorder, ReplicaReady};

// ------------------------------------------------------ the guards' rules

fn shadowed() -> MemoryFacts {
    MemoryFacts {
        shadow: MemoryShadowFacts {
            reconciled: true,
            promoted: false,
            backlog: 0,
            last_run: Some(("ok".into(), 0)),
            previous_frozen: false,
        },
        marked: Some(false),
        switched: Some(true),
        keep_previous: false,
    }
}

fn promoted() -> MemoryFacts {
    let mut facts = shadowed();
    facts.shadow.promoted = true;
    facts.marked = Some(true);
    facts
}

fn refusal(backend: &str, facts: &MemoryFacts) -> String {
    memories_may_open(backend, facts)
        .expect_err("refused")
        .to_string()
}

/// A vellev install that never ran a shadow opens as it always did: no
/// history here, nothing to guard.
#[test]
fn vellev_with_no_shadow_here_opens() {
    assert_eq!(memories_may_open("vellev", &MemoryFacts::default()), Ok(()));
    assert_eq!(
        memories_may_open("weaviate", &MemoryFacts::default()),
        Ok(())
    );
}

/// Memories shadowed here, and not promoted: vellev is refused, by the
/// reconcile's history, by a note left, or by either mark missing.
#[test]
fn vellev_is_refused_where_memories_were_shadowed_and_not_promoted() {
    assert!(refusal("vellev", &shadowed()).contains("promote memories"));
    let noted = MemoryFacts {
        shadow: MemoryShadowFacts {
            backlog: 2,
            ..MemoryShadowFacts::default()
        },
        ..MemoryFacts::default()
    };
    assert!(refusal("vellev", &noted).contains("not promoted"));
    let mut unmarked = promoted();
    unmarked.marked = Some(false);
    assert!(refusal("vellev", &unmarked).contains("not promoted"));
    let mut unrecorded = promoted();
    unrecorded.shadow.promoted = false;
    assert!(refusal("vellev", &unrecorded).contains("not promoted"));
}

/// Promoted, but with a note left for the replica: refused.
#[test]
fn vellev_is_refused_with_a_backlog() {
    let mut facts = promoted();
    facts.shadow.backlog = 1;
    assert!(refusal("vellev", &facts).contains("1 memories are still noted"));
}

/// Promoted, but the collection not marked as holding every memory:
/// refused, as opening it would copy into it.
#[test]
fn vellev_is_refused_with_an_unmarked_collection() {
    let mut facts = promoted();
    facts.switched = Some(false);
    assert!(refusal("vellev", &facts).contains("not marked as holding every memory"));
    facts.switched = None;
    assert!(refusal("vellev", &facts).contains("not marked"));
}

/// Promoted, but the last check of the replica was not clean: a fix, a
/// failure, or none recorded.
#[test]
fn vellev_is_refused_after_an_unclean_check() {
    for last in [
        Some(("ok".to_string(), 3)),
        Some(("failed".to_string(), 0)),
        Some(("refused_promoted".to_string(), 0)),
    ] {
        let mut facts = promoted();
        facts.shadow.last_run = last.clone();
        assert!(refusal("vellev", &facts).contains("not clean"), "{last:?}");
    }
}

/// The reconciles' rows are pruned after 180 days, and none runs once
/// memories are promoted: with none since the promotion, the record, made
/// only after a clean pass, stands for the last check.
#[test]
fn vellev_opens_with_no_reconcile_since_the_promotion() {
    let mut facts = promoted();
    facts.shadow.last_run = None;
    facts.shadow.reconciled = false;
    assert_eq!(memories_may_open("vellev", &facts), Ok(()));
}

/// The store memories switched from has missed erasures once memories
/// were promoted or a hub left it alone: weaviate and shadow are refused
/// keeping it again, whatever else holds.
#[test]
fn the_previous_store_is_never_kept_again_once_it_missed_erasures() {
    let mut facts = shadowed();
    facts.shadow.previous_frozen = true;
    facts.keep_previous = true;
    for backend in ["weaviate", "shadow"] {
        assert!(
            refusal(backend, &facts).contains("keep_previous = false"),
            "{backend}"
        );
    }
    assert_eq!(
        memories_may_open("vellev", &promoted()),
        Ok(()),
        "not vellev's"
    );
    facts.keep_previous = false;
    assert_eq!(memories_may_open("shadow", &facts), Ok(()));
    facts.keep_previous = true;
    facts.shadow.previous_frozen = false;
    assert_eq!(memories_may_open("weaviate", &facts), Ok(()), "never left");
}

#[test]
fn vellev_opens_once_promoted_clean_and_marked() {
    assert_eq!(memories_may_open("vellev", &promoted()), Ok(()));
}

/// Once promoted, Weaviate is behind: serving from it, or shadowing it
/// again, is refused until a rollback.
#[test]
fn weaviate_and_shadow_are_refused_once_promoted() {
    for backend in ["weaviate", "shadow"] {
        assert!(
            refusal(backend, &promoted()).contains("shadow rollback memories"),
            "{backend}"
        );
        assert_eq!(memories_may_open(backend, &shadowed()), Ok(()), "{backend}");
    }
}

fn artifacts(
    lease_set: bool,
    lease_held: bool,
    pending: i64,
    marked: Option<bool>,
) -> ArtifactFacts {
    ArtifactFacts {
        mirror: ArtifactMirrorFacts {
            lease_set,
            lease_held,
            pending,
            promoted: marked == Some(true),
        },
        marked,
    }
}

#[test]
fn the_artifact_index_is_served_from_vellev_only_once_its_shadow_is_promoted() {
    let refused = |facts: ArtifactFacts| artifacts_may_open("vellev", &facts).is_err();
    assert!(
        refused(artifacts(true, true, 0, Some(true))),
        "a lease held"
    );
    assert!(refused(artifacts(false, false, 4, None)), "notes left");
    assert!(
        refused(artifacts(true, false, 0, Some(false))),
        "shadowed, not promoted"
    );
    assert!(!refused(artifacts(true, false, 0, Some(true))), "promoted");
    // Promoted by one record and not the other: a lease ended by hand, or a
    // marker left by a restore.
    let mut half = artifacts(true, false, 0, Some(true));
    half.mirror.promoted = false;
    assert!(refused(half), "marked on vellev, not ended in Postgres");
    let mut half = artifacts(true, false, 0, Some(false));
    half.mirror.promoted = true;
    assert!(refused(half), "ended in Postgres, not marked on vellev");
    assert!(
        !refused(artifacts(false, false, 0, None)),
        "never shadowed, or ended"
    );
    assert_eq!(
        artifacts_may_open("weaviate", &artifacts(true, true, 9, None)),
        Ok(()),
        "Weaviate's keeper puts its own index right"
    );
}

#[test]
fn only_a_pass_promotes_and_too_few_only_the_artifact_index_on_the_owners_word() {
    let admits = |store, verdict: Verdict, accept| verdict_admits(store, &verdict, accept).is_ok();
    assert!(admits(MEMORIES, Verdict::Pass, false));
    assert!(admits(ARTIFACTS, Verdict::Pass, false));
    assert!(!admits(MEMORIES, Verdict::TooFewToJudge, true));
    assert!(!admits(ARTIFACTS, Verdict::TooFewToJudge, false));
    assert!(admits(ARTIFACTS, Verdict::TooFewToJudge, true));
    assert!(!admits(MEMORIES, Verdict::Fail("p99".into()), false));
    assert!(!admits(ARTIFACTS, Verdict::Missing("none".into()), true));
}

/// This release reads no verdicts: promotion fails closed on them.
#[tokio::test]
async fn no_verdict_is_read_so_none_admits() {
    for store in [MEMORIES, ARTIFACTS] {
        let verdict = Unrecorded.verdict(store).await.unwrap();
        assert!(verdict_admits(store, &verdict, true).is_err());
    }
}

/// A hub keeping the store memories switched from catches up from it and
/// never erases from it once memories are promoted: promotion is refused
/// until the shadow config leaves it alone, before anything is read.
#[tokio::test]
async fn memories_are_not_promoted_while_the_previous_store_is_kept() {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.memory.backend = "shadow".into();
    assert!(config.embeddings.keep_previous, "kept unless said");
    let err = promote_memories(&config, Arc::new(Verdicts(Verdict::Pass)), &quiet)
        .await
        .expect_err("refused");
    assert!(
        format!("{err:#}").contains("keep_previous = false"),
        "{err:#}"
    );
    config.embeddings.keep_previous = false;
    let err = promote_memories(&config, Arc::new(Verdicts(Verdict::Pass)), &quiet)
        .await
        .expect_err("no Postgres");
    assert!(!format!("{err:#}").contains("keep_previous"), "{err:#}");
}

/// A rollback onto a config that keeps the previous store would let the
/// hubs catch erased memories back up after it: refused, first.
#[tokio::test]
async fn memories_are_not_rolled_back_onto_a_config_that_keeps_the_previous_store() {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.memory.backend = "shadow".into();
    let err = rollback_memories(&config, &quiet)
        .await
        .expect_err("refused");
    assert!(
        format!("{err:#}").contains("keep_previous = false"),
        "{err:#}"
    );
    config.embeddings.keep_previous = false;
    let err = rollback_memories(&config, &quiet)
        .await
        .expect_err("no Postgres");
    assert!(!format!("{err:#}").contains("keep_previous"), "{err:#}");
}

// --------------------------------------------- promotion, step by step

struct Verdicts(Verdict);

#[async_trait]
impl VerdictSource for Verdicts {
    async fn verdict(&self, _store: &str) -> Result<Verdict> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct Fleet {
    busy: AtomicBool,
    asked: AtomicUsize,
}

#[async_trait]
impl super::Fleet for Fleet {
    async fn idle(&self) -> Result<()> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        match self.busy.load(Ordering::SeqCst) {
            true => Err(refused("a hub is running")),
            false => Ok(()),
        }
    }
}

type Writer = Box<dyn Fn(&Notes) + Send + Sync>;

/// Notes as Postgres keeps them, numbered; `during_drain` is called as the
/// notes are taken away, as a writer still running would act.
#[derive(Default)]
struct Notes {
    held: Mutex<Vec<(String, i64)>>,
    next: Mutex<i64>,
    during_drain: Mutex<Option<Writer>>,
}

impl Notes {
    fn note(&self, id: &str) {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        self.held.lock().unwrap().push((id.into(), *next));
    }
}

#[async_trait]
impl super::Notes for Notes {
    async fn last_seq(&self) -> Result<Option<i64>> {
        Ok(self.held.lock().unwrap().iter().map(|(_, seq)| *seq).max())
    }
    async fn drain_through(&self, seq: i64) -> Result<u64> {
        let writer = self.during_drain.lock().unwrap().take();
        if let Some(writer) = writer {
            writer(self);
        }
        let mut held = self.held.lock().unwrap();
        let before = held.len();
        held.retain(|(_, noted)| *noted > seq);
        Ok((before - held.len()) as u64)
    }
    async fn backlog(&self) -> Result<i64> {
        Ok(self.held.lock().unwrap().len() as i64)
    }
}

/// The record and the marker, and the order they were written in.
#[derive(Default)]
struct Marks {
    written: Mutex<Vec<&'static str>>,
}

#[async_trait]
impl super::Marks for Marks {
    async fn promote(&self) -> Result<()> {
        self.written.lock().unwrap().extend(["record", "marker"]);
        Ok(())
    }
    async fn state(&self) -> Result<(bool, bool)> {
        let written = self.written.lock().unwrap();
        Ok((written.contains(&"record"), written.contains(&"marker")))
    }
    async fn withdraw(&self) -> Result<()> {
        self.written.lock().unwrap().clear();
        Ok(())
    }
}

#[derive(Default)]
struct Recorded(Mutex<Vec<ReconcileRun>>);

#[async_trait]
impl Recorder for Recorded {
    async fn record(&self, run: &ReconcileRun) -> Result<()> {
        self.0.lock().unwrap().push(run.clone());
        Ok(())
    }
}

struct Ready;

#[async_trait]
impl ReplicaReady for Ready {
    async fn ready(&self) -> Result<()> {
        Ok(())
    }
}

fn id(n: u64) -> String {
    format!("00000000-0000-0000-0000-{n:012}")
}

fn memory(n: u64, content: &str) -> Value {
    json!({
        "id": id(n),
        "class": "Memories",
        "properties": { "content": content, "memoryId": format!("m{n}"), "deleted": false },
        "vector": [n as f32, 1.0],
    })
}

struct Promoting {
    fleet: Arc<Fleet>,
    notes: Arc<Notes>,
    marks: Arc<Marks>,
    replica: Arc<Held>,
    serving: Arc<Held>,
    recorded: Arc<Recorded>,
    promotion: MemoryPromotion,
}

/// A shadow a week in: Weaviate serving two memories, the replica missing
/// one of them, which a note names.
fn promoting(verdict: Verdict) -> Promoting {
    let replica = Arc::new(Held::holding(Some("hub"), vec![memory(1, "one")]));
    let serving = Arc::new(Held::holding(
        Some("hub"),
        vec![memory(1, "one"), memory(2, "two")],
    ));
    *serving.switched.lock().unwrap() = true;
    let recorded = Arc::new(Recorded::default());
    let fleet = Arc::new(Fleet::default());
    let notes = Arc::new(Notes::default());
    notes.note("m2");
    let marks = Arc::new(Marks::default());
    let promotion = MemoryPromotion {
        verdicts: Arc::new(Verdicts(verdict)),
        fleet: Arc::clone(&fleet) as Arc<dyn super::Fleet>,
        notes: Arc::clone(&notes) as Arc<dyn super::Notes>,
        reconcile: Arc::new(Reconcile {
            replica: Arc::clone(&replica) as Arc<dyn SyncTarget>,
            serving: Arc::clone(&serving) as Arc<dyn SyncTarget>,
            serving_name: "WorkforceMemory_hub".into(),
            ready: Arc::new(Ready),
            turns: Arc::new(NoTurns),
            recorder: Arc::clone(&recorded) as Arc<dyn Recorder>,
        }),
        replica: Arc::clone(&replica) as Arc<dyn SyncTarget>,
        marks: Arc::clone(&marks) as Arc<dyn super::Marks>,
    };
    Promoting {
        fleet,
        notes,
        marks,
        replica,
        serving,
        recorded,
        promotion,
    }
}

fn quiet(_: &str) {}

/// A promotion that passes: the replica in line, the notes it covered
/// gone, a second pass recorded clean (what the start-up guard reads as
/// the last check), the collection marked, and the record and the marker
/// written last.
#[tokio::test]
async fn a_promotion_brings_the_replica_in_line_checks_it_and_then_marks_it() {
    let p = promoting(Verdict::Pass);
    p.promotion.run(&quiet).await.unwrap();
    assert!(p.replica.get(&id(2)).is_some());
    assert_eq!(p.notes.held.lock().unwrap().len(), 0);
    {
        let runs = p.recorded.0.lock().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].copied, 1);
        let last = runs.last().unwrap();
        assert_eq!(
            (last.outcome, last.copied + last.updated + last.dropped),
            (ReconcileOutcome::Ok, 0)
        );
    }
    assert!(
        *p.replica.switched.lock().unwrap(),
        "marked as holding every memory"
    );
    assert_eq!(*p.marks.written.lock().unwrap(), vec!["record", "marker"]);
    assert_eq!(
        p.fleet.asked.load(Ordering::SeqCst),
        2,
        "asked again before marking"
    );
}

async fn refused_by(p: Promoting, why: &str) {
    let err = p.promotion.run(&quiet).await.expect_err("refused");
    assert!(err.downcast_ref::<Refused>().is_some(), "{err:#}");
    assert!(format!("{err:#}").contains(why), "{err:#}");
    assert!(p.marks.written.lock().unwrap().is_empty(), "{why}");
}

#[tokio::test]
async fn a_promotion_is_refused_while_a_hub_runs() {
    let p = promoting(Verdict::Pass);
    p.fleet.busy.store(true, Ordering::SeqCst);
    refused_by(p, "a hub is running").await;
}

#[tokio::test]
async fn a_promotion_is_refused_without_a_passing_verdict() {
    refused_by(promoting(Verdict::Missing("none".into())), "no verdict").await;
    refused_by(promoting(Verdict::TooFewToJudge), "too few to judge").await;
    refused_by(promoting(Verdict::Fail("L1".into())), "failed").await;
}

/// A memory noted after the last number was read: someone still writes.
#[tokio::test]
async fn a_promotion_is_refused_when_a_note_comes_after_the_last_pass() {
    let p = promoting(Verdict::Pass);
    *p.notes.during_drain.lock().unwrap() = Some(Box::new(|notes: &Notes| notes.note("m3")));
    refused_by(p, "still writes").await;
}

/// Weaviate changing between the two passes, unnoted: the second pass
/// fixes something, and promotion stops there.
#[tokio::test]
async fn a_promotion_is_refused_when_the_second_pass_is_not_clean() {
    let p = promoting(Verdict::Pass);
    let serving = Arc::clone(&p.serving);
    *p.notes.during_drain.lock().unwrap() = Some(Box::new(move |_: &Notes| {
        serving.objects.lock().unwrap().push(memory(4, "four"));
    }));
    refused_by(p, "second pass").await;
}

/// A reconcile that cannot run (the replica refuses writes) stops it.
#[tokio::test]
async fn a_promotion_is_refused_when_the_last_reconcile_fails() {
    let p = promoting(Verdict::Pass);
    *p.replica.promoted.lock().unwrap() = true;
    refused_by(p, "refused_promoted").await;
}

/// A promotion stopped between the record and the marker cannot be run
/// again as it stands (every replica write is refused): it says to roll
/// back, writing nothing. One already done says so.
#[tokio::test]
async fn a_promotion_part_done_or_done_is_refused_saying_what_to_do() {
    for (written, why) in [
        (vec!["record"], "stopped part way"),
        (vec!["marker"], "stopped part way"),
        (vec!["record", "marker"], "promoted to vellev already"),
    ] {
        let p = promoting(Verdict::Pass);
        *p.marks.written.lock().unwrap() = written.clone();
        let err = p.promotion.run(&quiet).await.expect_err("refused");
        assert!(format!("{err:#}").contains(why), "{written:?}: {err:#}");
        assert!(p.recorded.0.lock().unwrap().is_empty(), "nothing run");
        assert_eq!(*p.marks.written.lock().unwrap(), written);
    }
}

// ------------------------------------------------------------- rollback

struct Leftover {
    holds: AtomicBool,
    dropped: AtomicBool,
}

#[async_trait]
impl super::Leftover for Leftover {
    fn name(&self) -> String {
        "WorkforceMemory".into()
    }
    async fn empty(&self) -> Result<bool> {
        Ok(!self.holds.load(Ordering::SeqCst))
    }
    async fn drop_if_empty(&self) -> Result<bool> {
        if self.holds.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.dropped.store(true, Ordering::SeqCst);
        Ok(true)
    }
}

struct RollingBack {
    marks: Arc<Marks>,
    weaviate: Arc<Held>,
    promoted: Arc<Held>,
    first: Arc<Leftover>,
    rollback: MemoryRollback,
}

/// Promoted a while ago: vellev took a new memory, an edit and an erasure
/// that Weaviate, left behind, knows nothing of.
fn rolling_back() -> RollingBack {
    let promoted = Arc::new(Held::holding(
        Some("hub"),
        vec![
            memory(1, "one, edited on vellev"),
            memory(3, "new on vellev"),
        ],
    ));
    let weaviate = Arc::new(Held::holding(
        Some("hub"),
        vec![memory(1, "one"), memory(2, "erased on vellev")],
    ));
    *weaviate.switched.lock().unwrap() = true;
    let marks = Arc::new(Marks::default());
    marks.written.lock().unwrap().extend(["record", "marker"]);
    let first = Arc::new(Leftover {
        holds: AtomicBool::new(false),
        dropped: AtomicBool::new(false),
    });
    let rollback = MemoryRollback {
        fleet: Arc::new(Fleet::default()),
        marks: Arc::clone(&marks) as Arc<dyn super::Marks>,
        promoted: Arc::clone(&promoted) as Arc<dyn SyncSource>,
        weaviate: Arc::clone(&weaviate) as Arc<dyn SyncTarget>,
        ready: Arc::new(Ready),
        first: Arc::clone(&first) as Arc<dyn super::Leftover>,
    };
    RollingBack {
        marks,
        weaviate,
        promoted,
        first,
        rollback,
    }
}

fn sorted(store: &Held) -> Vec<Value> {
    let mut held = store.objects.lock().unwrap().clone();
    held.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    held
}

/// Everything written on vellev since the promotion is in Weaviate before
/// the promotion is taken away: new, edited and erased alike.
#[tokio::test]
async fn a_rollback_carries_back_what_vellev_took_since_and_then_unmarks() {
    let r = rolling_back();
    r.rollback.run(&quiet).await.unwrap();
    assert_eq!(sorted(&r.weaviate), sorted(&r.promoted));
    assert!(r.marks.written.lock().unwrap().is_empty(), "unmarked");
    assert!(r.first.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_rollback_never_drops_a_first_collection_holding_memories() {
    let r = rolling_back();
    r.first.holds.store(true, Ordering::SeqCst);
    let err = r.rollback.run(&quiet).await.expect_err("refused");
    assert!(format!("{err:#}").contains("holds memories"), "{err:#}");
    assert!(!r.first.dropped.load(Ordering::SeqCst));
    assert_eq!(r.marks.written.lock().unwrap().len(), 2, "still promoted");
    assert_eq!(
        r.weaviate.objects.lock().unwrap().len(),
        2,
        "nothing carried"
    );
}

/// A promotion stopped part way, either record without the other, is
/// rolled back as a whole one is.
#[tokio::test]
async fn a_promotion_part_done_is_rolled_back() {
    for written in [vec!["record"], vec!["marker"]] {
        let r = rolling_back();
        *r.marks.written.lock().unwrap() = written.clone();
        r.rollback.run(&quiet).await.unwrap();
        assert_eq!(sorted(&r.weaviate), sorted(&r.promoted), "{written:?}");
        assert!(r.marks.written.lock().unwrap().is_empty(), "{written:?}");
    }
}

/// vellev's store, taking a write after the first whole walk, as a hub
/// still running would: the second pass finds it.
struct Growing {
    inner: Arc<Held>,
    walks: AtomicUsize,
}

#[async_trait]
impl SyncSource for Growing {
    async fn export_page(&self, after: Option<&str>, with_vectors: bool) -> Result<Vec<Value>> {
        if after.is_none() && self.walks.fetch_add(1, Ordering::SeqCst) == 1 {
            self.inner
                .objects
                .lock()
                .unwrap()
                .push(memory(7, "written during the rollback"));
        }
        self.inner.export_page(after, with_vectors).await
    }
    fn embedding_model(&self) -> Option<&str> {
        self.inner.embedding_model()
    }
    async fn objects_of_memory(&self, memory_id: &str, with_vectors: bool) -> Result<Vec<Value>> {
        self.inner.objects_of_memory(memory_id, with_vectors).await
    }
    async fn objects_by_id(&self, ids: &[String], with_vectors: bool) -> Result<Vec<Value>> {
        self.inner.objects_by_id(ids, with_vectors).await
    }
}

/// Something still writing to vellev during a rollback: the second pass
/// is not clean, and the promotion stays.
#[tokio::test]
async fn a_rollback_whose_second_pass_is_not_clean_keeps_the_promotion() {
    let mut r = rolling_back();
    r.rollback.promoted = Arc::new(Growing {
        inner: Arc::clone(&r.promoted),
        walks: AtomicUsize::new(0),
    });
    let err = r.rollback.run(&quiet).await.expect_err("refused");
    assert!(format!("{err:#}").contains("second pass"), "{err:#}");
    assert_eq!(r.marks.written.lock().unwrap().len(), 2, "still promoted");
    assert!(!r.first.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn nothing_is_rolled_back_where_nothing_was_promoted() {
    let r = rolling_back();
    r.marks.written.lock().unwrap().clear();
    let err = r.rollback.run(&quiet).await.expect_err("refused");
    assert!(
        format!("{err:#}").contains("nothing to roll back"),
        "{err:#}"
    );
}

// ------------------------------------------------- against Postgres

mod postgres {
    use std::time::Duration;

    use workforce_common::postgres::PostgresConfig;
    use workforce_common::settings::WorkforceConfig;
    use workforce_common::shadow_postgres::{
        self, MirrorNotes, ReconcileOutcome, ReplicaLock, ShadowStore,
    };
    use workforce_common::telemetry_postgres::{run_telemetry_migrations, PostgresTelemetryStore};

    use super::super::*;
    use super::{Arc, ArtifactMirrorFacts, MemoryShadowFacts, Mutex, ReconcileRun};

    /// Run `test` on a fresh database, migrated, with a config naming it
    /// and a vellev node no one answers at.
    async fn on_postgres<F, Fut>(test: F)
    where
        F: FnOnce(WorkforceConfig, PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let seen: Arc<Mutex<Option<PostgresConfig>>> = Arc::default();
        let keep = Arc::clone(&seen);
        workforce_postgres::test_support::with_test_pool(
            move |cfg| {
                *keep.lock().unwrap() = Some(cfg.clone());
                PostgresTelemetryStore::build_pool(cfg)
            },
            |pool| async move {
                run_telemetry_migrations(&pool).await.unwrap();
                let mut config = WorkforceConfig::default();
                config.postgres = seen.lock().unwrap().clone();
                config.vellev.url = "http://127.0.0.1:1".into();
                config.vellev.api_key = Some("key".into());
                config.vellev.tenant = "acme".into();
                test(config, pool).await;
            },
        )
        .await;
    }

    async fn sql(pool: &PostgresPool, statement: &str) {
        pool.get()
            .await
            .unwrap()
            .batch_execute(statement)
            .await
            .unwrap();
    }

    fn run(outcome: ReconcileOutcome, copied: usize) -> ReconcileRun {
        ReconcileRun {
            store: MEMORIES.into(),
            started_at: chrono::Utc::now(),
            outcome,
            error: None,
            copied,
            updated: 0,
            dropped: 0,
            unchanged: 0,
            fixed: Vec::new(),
        }
    }

    /// The facts as Postgres holds them, the latest reconcile by order of
    /// recording.
    #[tokio::test]
    async fn the_memory_facts_are_read_as_recorded() {
        on_postgres(|_, pool| async move {
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert_eq!(facts, MemoryShadowFacts::default());
            let store = ShadowStore::with_pool(pool.clone()).await.unwrap();
            store
                .record_reconcile(&run(ReconcileOutcome::Ok, 5))
                .await
                .unwrap();
            store
                .record_reconcile(&run(ReconcileOutcome::Ok, 0))
                .await
                .unwrap();
            MirrorNotes::with_pool(pool.clone())
                .await
                .unwrap()
                .note(&["m1".into()])
                .await
                .unwrap();
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert_eq!(
                facts,
                MemoryShadowFacts {
                    reconciled: true,
                    promoted: false,
                    backlog: 1,
                    last_run: Some(("ok".into(), 0)),
                    previous_frozen: false,
                }
            );
            ReplicaLock::new(pool.clone())
                .record_promotion("C")
                .await
                .unwrap();
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert!(facts.promoted);
        })
        .await;
    }

    /// A process on Weaviate is refused once Postgres records memories as
    /// promoted; one on vellev, where memories were shadowed, is refused
    /// when vellev cannot be asked whether they are: never let through on
    /// facts not read.
    #[tokio::test]
    async fn the_guard_refuses_on_the_record_and_when_it_cannot_read() {
        on_postgres(|mut config, pool| async move {
            config.memory.backend = "weaviate".into();
            guard_memories(&config).await.unwrap();
            ShadowStore::with_pool(pool.clone())
                .await
                .unwrap()
                .record_reconcile(&run(ReconcileOutcome::Ok, 0))
                .await
                .unwrap();
            guard_memories(&config).await.unwrap();

            config.memory.backend = "vellev".into();
            let err = guard_memories(&config).await.expect_err("vellev unread");
            assert!(format!("{err:#}").contains("cannot be read"), "{err:#}");

            config.memory.backend = "weaviate".into();
            config.embeddings.keep_previous = false;
            ReplicaLock::new(pool.clone())
                .record_promotion("C")
                .await
                .unwrap();
            let err = guard_memories(&config).await.expect_err("promoted");
            assert!(format!("{err:#}").contains("rollback"), "{err:#}");
            // "sqlite" with Weaviate enabled is served from Weaviate.
            config.memory.backend = "sqlite".into();
            config.weaviate.enabled = true;
            assert!(guard_memories(&config).await.is_err());
        })
        .await;
    }

    /// Postgres unreachable: refused, not let through.
    #[tokio::test]
    async fn the_guard_refuses_when_postgres_cannot_be_read() {
        let mut config = WorkforceConfig::default();
        config.memory.backend = "weaviate".into();
        config.postgres = Some(PostgresConfig {
            enabled: true,
            url: Some("postgres://nobody@127.0.0.1:1/none".into()),
            connect_timeout_secs: 1,
            ..PostgresConfig::default()
        });
        let err = tokio::time::timeout(Duration::from_secs(60), guard_memories(&config))
            .await
            .expect("bounded")
            .expect_err("refused");
        assert!(format!("{err:#}").contains("cannot be read"), "{err:#}");
    }

    /// The promotion's record is written before vellev's marker: with
    /// vellev not answering, the promotion fails, and Postgres already
    /// refuses every write into the replica.
    #[tokio::test]
    async fn the_record_is_written_before_the_marker() {
        on_postgres(|_, pool| async move {
            let marks = ReplicaMarks {
                lock: Arc::new(ReplicaLock::new(pool.clone())),
                replica: VellevStore::new("http://127.0.0.1:1", "key", "acme", "C").unwrap(),
            };
            assert!(marks.promote().await.is_err(), "vellev does not answer");
            let lock = ReplicaLock::new(pool);
            assert!(lock.promotion_recorded().await.unwrap());
            let held = lock
                .shared(Duration::from_secs(5), Duration::from_secs(5))
                .await
                .unwrap();
            assert!(held.promoted, "a reconcile page is refused from now on");
            held.release().await.unwrap();
        })
        .await;
    }

    /// What happened to the store memories switched from is kept for good:
    /// a promotion, kept past its rollback; a hub that left it alone,
    /// recorded as it opens memories; and either refuses keeping it again.
    #[tokio::test]
    async fn the_previous_store_s_history_is_kept_for_good() {
        on_postgres(|mut config, pool| async move {
            let lock = ReplicaLock::new(pool.clone());
            lock.record_promotion("C").await.unwrap();
            lock.withdraw_promotion().await.unwrap();
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert_eq!((facts.promoted, facts.previous_frozen), (false, true));
            config.memory.backend = "weaviate".into();
            let err = guard_memories(&config).await.expect_err("kept again");
            assert!(
                format!("{err:#}").contains("keep_previous = false"),
                "{err:#}"
            );
            config.embeddings.keep_previous = false;
            guard_memories(&config).await.unwrap();
        })
        .await;
        on_postgres(|mut config, pool| async move {
            config.memory.backend = "shadow".into();
            config.embeddings.keep_previous = false;
            guard_memories(&config).await.unwrap();
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert!(facts.previous_frozen, "left alone, recorded");
            config.embeddings.keep_previous = true;
            assert!(guard_memories(&config).await.is_err());
        })
        .await;
    }

    /// The notes a whole sync covered go, and one noted after stays.
    #[tokio::test]
    async fn notes_are_drained_only_up_to_the_number_read_before() {
        on_postgres(|_, pool| async move {
            let notes = MirrorNotes::with_pool(pool).await.unwrap();
            assert_eq!(notes.last_seq().await.unwrap(), None);
            notes.note(&["a".into(), "b".into()]).await.unwrap();
            let seen = notes.last_seq().await.unwrap().expect("noted");
            notes.note(&["c".into()]).await.unwrap();
            assert_eq!(notes.drain_through(seen).await.unwrap(), 2);
            assert_eq!(notes.backlog().await.unwrap().pending, 1);
        })
        .await;
    }

    /// A hub's keeper holds its index's serving lock shared for as long as
    /// it runs: the fleet is not idle while it does, nor while a mirror's
    /// lease holds. Once idle, the fleet holds the locks to its end, so a
    /// hub started meanwhile takes none.
    // A hub's own session is made with deadpool_postgres, a dependency of
    // the hub's `postgres` feature.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn the_fleet_is_busy_while_a_hub_holds_a_serving_lock_and_holds_them_after() {
        on_postgres(|_, pool| async move {
            let shared = |backend: &'static str| {
                let pool = pool.clone();
                async move {
                    let hub = deadpool_postgres::Object::take(pool.get().await.unwrap());
                    let took: bool = hub
                        .query_one(
                            "SELECT pg_try_advisory_lock_shared($1)",
                            &[&workforce_orchestrator::artifact_store::serving_lock(
                                backend,
                            )],
                        )
                        .await
                        .unwrap()
                        .get(0);
                    (hub, took)
                }
            };
            for backend in ["weaviate", "vellev"] {
                let (hub, took) = shared(backend).await;
                assert!(took);
                let err = PostgresFleet::new(pool.clone())
                    .idle()
                    .await
                    .expect_err(backend);
                assert!(format!("{err:#}").contains(backend), "{err:#}");
                drop(hub);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let fleet = PostgresFleet::new(pool.clone());
            fleet.idle().await.unwrap();
            for backend in ["weaviate", "vellev"] {
                let (_hub, took) = shared(backend).await;
                assert!(
                    !took,
                    "a hub started during promotion takes no {backend} lock"
                );
            }
            fleet.idle().await.unwrap();
            drop(fleet);
            // A dropped connection is closed in the background: its locks
            // go within moments.
            let mut took = false;
            for _ in 0..50 {
                let (hub, now) = shared("vellev").await;
                drop(hub);
                if now {
                    took = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(took, "let go at the end");
            tokio::time::sleep(Duration::from_millis(500)).await;
            sql(&pool, "SELECT artifact_mirror_renew('90 seconds')").await;
            let err = PostgresFleet::new(pool.clone())
                .idle()
                .await
                .expect_err("lease");
            assert!(format!("{err:#}").contains("lease"), "{err:#}");
        })
        .await;
    }

    /// The reconciles' rows pruned: a promoted store's last check is the
    /// promotion's own; a reconcile since that is not clean is seen.
    #[tokio::test]
    async fn the_last_check_is_read_since_the_promotion() {
        on_postgres(|_, pool| async move {
            let store = ShadowStore::with_pool(pool.clone()).await.unwrap();
            store
                .record_reconcile(&run(ReconcileOutcome::Ok, 4))
                .await
                .unwrap();
            ReplicaLock::new(pool.clone())
                .record_promotion("C")
                .await
                .unwrap();
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert_eq!((facts.promoted, facts.last_run.clone()), (true, None));
            sql(&pool, "DELETE FROM shadow_reconciles").await;
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert_eq!((facts.reconciled, facts.last_run), (false, None));
            store
                .record_reconcile(&run(ReconcileOutcome::Failed, 0))
                .await
                .unwrap();
            let facts = shadow_postgres::memory_shadow_facts(&pool, MEMORIES)
                .await
                .unwrap();
            assert_eq!(facts.last_run, Some(("failed".into(), 0)));
        })
        .await;
    }

    /// With Postgres there and no promotion, a shadow config is let through
    /// the guard, and fails on Weaviate alone.
    #[tokio::test]
    async fn a_shadow_with_postgres_there_fails_on_weaviate_alone() {
        on_postgres(|mut config, _| async move {
            config.memory.backend = "shadow".into();
            config.weaviate.enabled = true;
            config.embeddings.provider = workforce_common::settings::EmbeddingsProvider::Tei;
            let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            config.weaviate.url = format!("http://{}", closed.local_addr().unwrap());
            drop(closed);
            let failed = format!(
                "{:#}",
                crate::state::resolve_memory_store_with_budget(&config, Duration::ZERO)
                    .await
                    .err()
                    .expect("no Weaviate to reach")
            );
            assert!(!failed.contains("cannot be read"), "{failed}");
            assert!(failed.contains("Weaviate"), "{failed}");
        })
        .await;
    }

    /// Ending the mirror ends the lease, so nothing is noted after, and
    /// takes every note away.
    #[tokio::test]
    async fn ending_the_artifact_mirror_ends_its_lease_and_its_notes() {
        on_postgres(|_, pool| async move {
            sql(&pool, "SELECT artifact_mirror_renew('90 seconds')").await;
            sql(
                &pool,
                "INSERT INTO artifact_index_pending_mirror (artifact_id, text_hash) VALUES ('a', 'h')",
            )
            .await;
            let facts = shadow_postgres::artifact_mirror_facts(&pool).await.unwrap();
            assert_eq!(
                (facts.lease_set, facts.lease_held, facts.pending),
                (true, true, 1)
            );
            assert_eq!(
                shadow_postgres::end_artifact_mirror(&pool).await.unwrap(),
                1
            );
            let facts = shadow_postgres::artifact_mirror_facts(&pool).await.unwrap();
            assert_eq!(
                facts,
                ArtifactMirrorFacts {
                    lease_set: true,
                    lease_held: false,
                    pending: 0,
                    promoted: true,
                }
            );
            // No write is noted for the mirror once it is ended.
            sql(
                &pool,
                "INSERT INTO artifacts (id, artifact_type, title, content, owner_id, created_by,
                     last_modified_by, created_at, updated_at)
                 VALUES ('b', 'notes', 't', 'c', 'o', 'o', 'o', now(), now())",
            )
            .await;
            let facts = shadow_postgres::artifact_mirror_facts(&pool).await.unwrap();
            assert_eq!(facts.pending, 0);
            // A renewal takes the promotion's flag away: a shadow kept
            // again means the promotion no longer stands.
            shadow_postgres::end_artifact_mirror(&pool).await.unwrap();
            sql(&pool, "SELECT artifact_mirror_renew('90 seconds')").await;
            let facts = shadow_postgres::artifact_mirror_facts(&pool).await.unwrap();
            assert_eq!(
                (facts.promoted, facts.lease_held),
                (false, true),
                "renewed over the flag"
            );
            shadow_postgres::end_artifact_mirror(&pool).await.unwrap();
            shadow_postgres::unend_artifact_mirror(&pool).await.unwrap();
            let facts = shadow_postgres::artifact_mirror_facts(&pool).await.unwrap();
            assert_eq!(facts, ArtifactMirrorFacts::default());
        })
        .await;
    }
}
