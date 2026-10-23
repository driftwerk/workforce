//! Tests of the memory write mirror (DW-676, PR 7b).

use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

use futures::FutureExt;
use serde_json::Value;
use workforce_core::types::AgentId;
use workforce_memory::admin::testing::Held;
use workforce_memory::admin::{NoTurns, SyncMode};
use workforce_memory::in_memory::InMemoryStore;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer, MemorySearchResult, Visibility};

use super::*;
use crate::memory_erasure::{Erases, ErasureLock, Held as LockHeld, ShadowedMemory};

fn agent() -> AgentId {
    AgentId::new("agent")
}

/// An object of memory `memory` as Weaviate gives it.
fn object(id: &str, memory: &str, content: &str) -> Value {
    serde_json::json!({
        "id": id,
        "class": "Memories",
        "properties": { "content": content, "memoryId": memory, "deleted": false, "accessCount": 0 },
    })
}

const O1: &str = "00000000-0000-0000-0000-000000000001";
const O2: &str = "00000000-0000-0000-0000-000000000002";

// ---- the noter -------------------------------------------------------

/// A sink that keeps what it is given.
#[derive(Default)]
struct Kept(Mutex<Vec<String>>);

#[async_trait]
impl NoteSink for Kept {
    async fn note(&self, ids: &[String]) -> Result<()> {
        self.0.lock().unwrap().extend(ids.iter().cloned());
        Ok(())
    }
}

impl Kept {
    /// What it was given, once `n` ids are in, sorted.
    async fn after(&self, n: usize) -> Vec<String> {
        for _ in 0..200 {
            if self.0.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut kept = self.0.lock().unwrap().clone();
        kept.sort();
        kept
    }
}

/// A sink that never answers: Postgres hung.
struct Hung;

#[async_trait]
impl NoteSink for Hung {
    async fn note(&self, _: &[String]) -> Result<()> {
        std::future::pending().await
    }
}

/// A lock always free.
struct Free;
struct Let;

#[async_trait]
impl LockHeld for Let {
    async fn release(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ErasureLock for Free {
    async fn hold(&self) -> Result<Box<dyn LockHeld>> {
        Ok(Box::new(Let))
    }
}

/// A replica a purge never reaches in these tests.
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

/// Weaviate's store as the wrapper sees it: the in-memory store, failing
/// writes or a lookup of an identity's memories while told to, and saying
/// what it was asked, in order.
#[derive(Default)]
struct Spy {
    inner: InMemoryStore,
    failing: std::sync::atomic::AtomicBool,
    lookup_fails: std::sync::atomic::AtomicBool,
    asked: Mutex<Vec<&'static str>>,
}

impl Spy {
    fn asked(&self, what: &'static str) {
        self.asked.lock().unwrap().push(what);
    }
    fn up(&self) -> Result<()> {
        match self.failing.load(Ordering::SeqCst) {
            true => Err(eyre::eyre!("Weaviate answered 500")),
            false => Ok(()),
        }
    }
}

#[async_trait]
impl MemoryStore for Spy {
    async fn remember(&self, memory: Memory) -> Result<MemoryId> {
        self.up()?;
        self.inner.remember(memory).await
    }
    async fn recall(&self, query: &str, filter: MemoryFilter) -> Result<Vec<MemorySearchResult>> {
        self.inner.recall(query, filter).await
    }
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>> {
        self.inner.get(id).await
    }
    async fn forget(&self, id: &MemoryId) -> Result<()> {
        self.up()?;
        self.inner.forget(id).await
    }
    async fn share(&self, id: &MemoryId, visibility: Visibility) -> Result<()> {
        self.up()?;
        self.inner.share(id, visibility).await
    }
    async fn count(&self, filter: MemoryFilter) -> Result<usize> {
        self.inner.count(filter).await
    }
    async fn list(&self, filter: MemoryFilter) -> Result<Vec<Memory>> {
        self.inner.list(filter).await
    }
    async fn update_tags(&self, id: &MemoryId, tags: Vec<String>) -> Result<()> {
        self.up()?;
        self.inner.update_tags(id, tags).await
    }
    async fn adjust_importance(&self, id: &MemoryId, delta: f32) -> Result<f32> {
        self.up()?;
        self.inner.adjust_importance(id, delta).await
    }
    async fn identity_memories(&self, owner: &AgentId, identity: &str) -> Result<Vec<MemoryId>> {
        self.asked("identity_memories");
        if self.lookup_fails.load(Ordering::SeqCst) {
            return Err(eyre::eyre!("Weaviate answered 500"));
        }
        self.inner.identity_memories(owner, identity).await
    }
    async fn identity_counts(&self, owner: &AgentId) -> Result<BTreeMap<String, usize>> {
        self.inner.identity_counts(owner).await
    }
    async fn reassign_identity(&self, owner: &AgentId, from: &str, to: &str) -> Result<usize> {
        self.asked("reassign_identity");
        self.up()?;
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
    async fn record_recurrence(
        &self,
        id: &MemoryId,
        additional_message_ids: Vec<String>,
    ) -> Result<u32> {
        self.up()?;
        self.inner
            .record_recurrence(id, additional_message_ids)
            .await
    }
}

fn wrapped(serving: Arc<Spy>, noter: Noter) -> ShadowedMemory {
    ShadowedMemory::new(serving, Arc::new(NoReplica), Arc::new(Free)).noting(noter)
}

/// Test 1. The user's write never waits on the shadow: with the noter's
/// writes hung and its channel full, each write still finishes on its
/// first poll, and what could not be noted is counted.
#[tokio::test]
async fn a_hung_noter_never_slows_a_write_and_counts_what_it_drops() {
    let noter = spawn_noter(Arc::new(Hung), 1, 1);
    let serving = Arc::new(Spy::default());
    let store = wrapped(Arc::clone(&serving), noter.clone());
    for n in 0..50 {
        let memory = Memory::new(format!("note {n}"), MemoryLayer::Episodic, agent());
        let id = store
            .remember(memory)
            .now_or_never()
            .expect("the write finished at once")
            .unwrap();
        store
            .forget(&id)
            .now_or_never()
            .expect("the write finished at once")
            .unwrap();
        // Let the noter's task take what it can.
        tokio::task::yield_now().await;
    }
    let dropped = noter.counts().dropped.load(Ordering::SeqCst);
    assert!(dropped >= 97, "dropped {dropped} of 100");
}

/// Test 2 and the table of methods. Each write Weaviate took is noted,
/// once it took it; a write it refused is not; a read never is.
#[tokio::test]
async fn writes_weaviate_took_are_noted_and_nothing_else() {
    let kept = Arc::new(Kept::default());
    let serving = Arc::new(Spy::default());
    let store = wrapped(
        Arc::clone(&serving),
        spawn_noter(Arc::clone(&kept) as Arc<dyn NoteSink>, 64, 8),
    );
    let mut ids = Vec::new();
    for n in 0..7 {
        let memory = Memory::new(format!("note {n}"), MemoryLayer::Episodic, agent())
            .with_identity_id("bob");
        ids.push(serving.inner.remember(memory).await.unwrap());
    }
    // Reads.
    store.recall("note", MemoryFilter::default()).await.unwrap();
    store.get(&ids[0]).await.unwrap();
    store.count(MemoryFilter::default()).await.unwrap();
    store.list(MemoryFilter::default()).await.unwrap();
    store.identity_memories(&agent(), "bob").await.unwrap();
    store.identity_counts(&agent()).await.unwrap();
    store.identities(&agent()).await.unwrap();
    store
        .find_near_duplicate("note", MemoryFilter::default(), 0.9)
        .await
        .unwrap();
    // Writes refused by Weaviate.
    serving.failing.store(true, Ordering::SeqCst);
    assert!(store.forget(&ids[6]).await.is_err());
    assert!(store
        .remember(Memory::new("refused", MemoryLayer::Episodic, agent()))
        .await
        .is_err());
    serving.failing.store(false, Ordering::SeqCst);
    // Writes taken.
    let remembered = store
        .remember(Memory::new("new", MemoryLayer::Episodic, agent()))
        .await
        .unwrap();
    store.forget(&ids[0]).await.unwrap();
    store
        .share(&ids[1], Visibility::Team("t".into()))
        .await
        .unwrap();
    store.update_tags(&ids[2], vec!["t".into()]).await.unwrap();
    store.adjust_importance(&ids[3], 0.1).await.unwrap();
    store
        .record_recurrence(&ids[4], vec!["m".into()])
        .await
        .unwrap();
    let mut want: Vec<String> = ids[..5].iter().map(|id| id.0.clone()).collect();
    want.push(remembered.0);
    want.sort();
    assert_eq!(kept.after(want.len()).await, want);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(kept.0.lock().unwrap().len(), want.len(), "nothing else");
}

/// Test 8. A reassign's memories are resolved from Weaviate before it
/// (after, they are filed under the new identity among others) and all
/// noted once it is made, in one place of the channel.
#[tokio::test]
async fn a_reassign_resolves_its_memories_first_and_notes_them_all() {
    let kept = Arc::new(Kept::default());
    let serving = Arc::new(Spy::default());
    let mut moved = Vec::new();
    for n in 0..3 {
        let memory =
            Memory::new(format!("bob {n}"), MemoryLayer::Episodic, agent()).with_identity_id("bob");
        moved.push(serving.inner.remember(memory).await.unwrap().0);
    }
    serving
        .inner
        .remember(Memory::new("carol", MemoryLayer::Episodic, agent()).with_identity_id("carol"))
        .await
        .unwrap();
    // One place for all three: a channel of one holds the reassign.
    let store = wrapped(
        Arc::clone(&serving),
        spawn_noter(Arc::clone(&kept) as Arc<dyn NoteSink>, 1, 256),
    );
    assert_eq!(
        store
            .reassign_identity(&agent(), "bob", "carol")
            .await
            .unwrap(),
        3
    );
    moved.sort();
    assert_eq!(kept.after(3).await, moved);
    assert_eq!(
        serving.asked.lock().unwrap().clone(),
        vec!["identity_memories", "reassign_identity"]
    );
}

/// Q3. Resolving a reassign's memories failing does not fail the reassign:
/// it is made, and counted as dropped for the reconcile to find.
#[tokio::test]
async fn a_reassign_whose_memories_cannot_be_resolved_is_made_and_counted() {
    let serving = Arc::new(Spy::default());
    serving
        .inner
        .remember(Memory::new("bob", MemoryLayer::Episodic, agent()).with_identity_id("bob"))
        .await
        .unwrap();
    serving.lookup_fails.store(true, Ordering::SeqCst);
    let noter = spawn_noter(Arc::new(Kept::default()), 8, 8);
    let store = wrapped(Arc::clone(&serving), noter.clone());
    assert_eq!(
        store
            .reassign_identity(&agent(), "bob", "carol")
            .await
            .unwrap(),
        1
    );
    assert_eq!(noter.counts().dropped.load(Ordering::SeqCst), 1);
}

/// A config whose memories are shadowed, and whose noter can never be
/// made ready (no `[postgres]`).
fn shadow_config(paused: bool) -> workforce_common::settings::WorkforceConfig {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.memory.backend = workforce_common::settings::SHADOWED.into();
    config.vellev.shadow.paused = paused;
    config
}

/// Review finding 3. A process makes one noter however many stores it
/// opens; a store opened while the shadow is paused, or by a CLI process,
/// has none and makes none.
#[tokio::test]
async fn a_process_makes_one_noter_and_none_paused_or_for_the_cli() {
    use crate::memory_erasure::{Noters, Noting};
    let noters = Noters::new();
    assert!(noters
        .for_store(&shadow_config(true), Noting::Hub)
        .is_none());
    assert!(noters
        .for_store(&shadow_config(false), Noting::Off)
        .is_none());
    assert_eq!(noters.made(), 0, "paused, or the CLI: none made");
    let config = shadow_config(false);
    for _ in 0..3 {
        assert!(noters.for_store(&config, Noting::Hub).is_some());
    }
    assert_eq!(noters.made(), 1, "one noter a process");
}

/// Review finding 2. A noter whose list cannot be made ready is made at
/// once all the same, never waited on; it says so once, counts it, is not
/// made again, and drops what it is given.
#[tokio::test]
async fn a_noter_that_cannot_be_made_ready_fails_for_good_and_never_waits() {
    use crate::memory_erasure::{Noters, Noting};
    let noters = Noters::new();
    let config = shadow_config(false);
    // Not async: making it cannot wait on Postgres.
    let noter = noters.for_store(&config, Noting::Hub).unwrap();
    let counts = noter.counts();
    for _ in 0..200 {
        if counts.setup_failed.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(counts.setup_failed.load(Ordering::SeqCst), 1);
    noter.note(Note::Many(vec![MemoryId("a".into()), MemoryId("b".into())]));
    for _ in 0..200 {
        if counts.dropped.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    assert!(noters.for_store(&config, Noting::Hub).is_some());
    assert_eq!(noters.made(), 1, "not made again");
}

/// Review finding 2. Making a noter's list ready is bounded: one that
/// hangs is given up after the limit, and writes meanwhile never wait.
#[tokio::test(start_paused = true)]
async fn a_noter_whose_list_hangs_gives_up_at_its_limit() {
    let noter = spawn_noter_after(
        std::future::pending::<Result<Arc<dyn NoteSink>>>(),
        Duration::from_secs(30),
        4,
        4,
    );
    let serving = Arc::new(Spy::default());
    let store = wrapped(Arc::clone(&serving), noter.clone());
    for n in 0..10 {
        store
            .remember(Memory::new(
                format!("n {n}"),
                MemoryLayer::Episodic,
                agent(),
            ))
            .now_or_never()
            .expect("the write finished at once")
            .unwrap();
    }
    let counts = noter.counts();
    tokio::time::sleep(Duration::from_secs(29)).await;
    assert_eq!(counts.setup_failed.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(counts.setup_failed.load(Ordering::SeqCst), 1);
    assert_eq!(counts.written.load(Ordering::SeqCst), 0);
}

/// Test 3 (noting). A hub notes unless paused; a CLI process never.
#[test]
fn a_noter_is_made_for_a_hub_not_paused_alone() {
    use crate::memory_erasure::{wants_noter, Noting};
    assert!(wants_noter(Noting::Hub, false));
    assert!(!wants_noter(Noting::Hub, true));
    assert!(!wants_noter(Noting::Off, false));
    assert!(!wants_noter(Noting::Off, true));
}

// ---- the applier -----------------------------------------------------

/// The pending list in memory, numbering notes as Postgres does.
#[derive(Default)]
struct List {
    rows: Mutex<BTreeMap<String, Row>>,
    seq: AtomicUsize,
}

#[derive(Debug, Clone)]
struct Row {
    seq: i64,
    tries: i32,
    error: Option<String>,
    next: Option<DateTime<Utc>>,
}

impl List {
    fn note(&self, id: &str) -> i64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) as i64 + 1;
        self.rows.lock().unwrap().insert(
            id.to_string(),
            Row {
                seq,
                tries: 0,
                error: None,
                next: None,
            },
        );
        seq
    }
    fn row(&self, id: &str) -> Option<Row> {
        self.rows.lock().unwrap().get(id).cloned()
    }
}

#[async_trait]
impl PendingNotes for List {
    async fn due(
        &self,
        now: DateTime<Utc>,
        after: Option<&DueNote>,
        limit: i64,
    ) -> Result<Vec<DueNote>> {
        // As Postgres orders them: NULL first, then by when, then by id.
        let key = |next: Option<DateTime<Utc>>, id: &str| (next.is_some(), next, id.to_string());
        let mut due: Vec<DueNote> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, row)| row.next.is_none_or(|n| n <= now))
            .map(|(id, row)| DueNote {
                id: id.clone(),
                next_try_at: row.next,
            })
            .filter(|d| after.is_none_or(|a| key(d.next_try_at, &d.id) > key(a.next_try_at, &a.id)))
            .collect();
        due.sort_by_key(|d| key(d.next_try_at, &d.id));
        due.truncate(limit as usize);
        Ok(due)
    }
    async fn seen(&self, id: &str) -> Result<Option<Noted>> {
        Ok(self.row(id).map(|row| Noted {
            seq: row.seq,
            tries: row.tries,
        }))
    }
    async fn taken(&self, id: &str, seq: i64) -> Result<bool> {
        let mut rows = self.rows.lock().unwrap();
        match rows.get(id) {
            Some(row) if row.seq == seq => {
                rows.remove(id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
    async fn renoted(
        &self,
        id: &str,
        seq: i64,
        error: &str,
        _now: DateTime<Utc>,
        next: DateTime<Utc>,
    ) -> Result<()> {
        let new = self.seq.fetch_add(1, Ordering::SeqCst) as i64 + 1;
        let mut rows = self.rows.lock().unwrap();
        let row = rows.entry(id.to_string()).or_insert(Row {
            seq,
            tries: 0,
            error: None,
            next: None,
        });
        if row.seq == seq {
            row.tries += 1;
            row.error = Some(error.to_string());
            row.next = Some(next);
        }
        row.seq = new;
        Ok(())
    }
}

/// vellev's replica in memory, whose writes can be held at a gate, as a
/// request already sent that the node takes when it takes it: a write
/// let through lands whether or not its caller still waits for it.
#[derive(Default)]
struct Gated {
    inner: Arc<Held>,
    /// Holds the next write until opened.
    gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// Told when a write reaches the gate.
    arrived: Arc<tokio::sync::Notify>,
    down: std::sync::atomic::AtomicBool,
    /// What happened, in order: each write landing, by object id.
    events: Arc<Mutex<Vec<String>>>,
}

impl Gated {
    fn holding(objects: Vec<Value>) -> Self {
        Self {
            inner: Arc::new(Held::holding(None, objects)),
            ..Self::default()
        }
    }

    /// Hold the next write; open it with what is returned.
    fn close(&self) -> tokio::sync::oneshot::Sender<()> {
        let (open, gate) = tokio::sync::oneshot::channel();
        *self.gate.lock().unwrap() = Some(gate);
        open
    }

    fn up(&self) -> Result<()> {
        match self.down.load(Ordering::SeqCst) {
            true => Err(eyre::eyre!("vellev answered 503")),
            false => Ok(()),
        }
    }

    fn content(&self, id: &str) -> Option<String> {
        self.inner
            .get(id)
            .map(|o| o["properties"]["content"].as_str().unwrap().to_string())
    }
}

#[async_trait]
impl SyncSource for Gated {
    async fn export_page(&self, after: Option<&str>, with_vectors: bool) -> Result<Vec<Value>> {
        self.inner.export_page(after, with_vectors).await
    }
    fn embedding_model(&self) -> Option<&str> {
        self.inner.embedding_model()
    }
    async fn objects_of_memory(&self, memory_id: &str, with_vectors: bool) -> Result<Vec<Value>> {
        self.up()?;
        self.inner.objects_of_memory(memory_id, with_vectors).await
    }
    async fn objects_by_id(&self, ids: &[String], with_vectors: bool) -> Result<Vec<Value>> {
        self.inner.objects_by_id(ids, with_vectors).await
    }
}

#[async_trait]
impl SyncTarget for Gated {
    async fn upsert(&self, objects: &[Value]) -> Result<usize> {
        self.up()?;
        let gate = self.gate.lock().unwrap().take();
        let inner = Arc::clone(&self.inner);
        let events = Arc::clone(&self.events);
        let objects = objects.to_vec();
        let arrived = Arc::clone(&self.arrived);
        let sent = tokio::spawn(async move {
            if let Some(gate) = gate {
                arrived.notify_one();
                let _ = gate.await;
            }
            for object in &objects {
                events
                    .lock()
                    .unwrap()
                    .push(format!("written {}", object["id"].as_str().unwrap()));
            }
            inner.upsert(&objects).await
        });
        sent.await?
    }
    async fn patch(&self, id: &str, properties: Value) -> Result<()> {
        self.up()?;
        self.events.lock().unwrap().push(format!("patched {id}"));
        self.inner.patch(id, properties).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.up()?;
        self.inner.delete(id).await
    }
    async fn vectors(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        self.inner.vectors(texts).await
    }
    async fn switched(&self) -> Result<bool> {
        self.inner.switched().await
    }
    async fn mark_switched(&self) -> Result<()> {
        self.inner.mark_switched().await
    }
    async fn promoted(&self) -> Result<bool> {
        self.inner.promoted().await
    }
}

fn now() -> DateTime<Utc> {
    "2026-10-21T12:00:00Z".parse().unwrap()
}

fn applier(
    replica: Arc<Gated>,
    serving: Arc<Held>,
    notes: Arc<List>,
    turns: Arc<dyn WriteTurns>,
) -> Applier {
    Applier {
        replica,
        serving,
        notes,
        turns,
        clock: Arc::new(now),
    }
}

/// Test 4. A note is taken away only at the number read before applying:
/// a write noted meanwhile is a later one's, and stays to be applied.
#[tokio::test]
async fn a_note_made_while_applying_stays() {
    let serving = Arc::new(Held::holding(None, vec![object(O1, "m1", "one")]));
    let replica = Arc::new(Gated::default());
    let notes = Arc::new(List::default());
    notes.note("m1");
    let open = replica.close();
    let applier = Arc::new(applier(
        Arc::clone(&replica),
        serving,
        Arc::clone(&notes),
        Arc::new(NoTurns),
    ));
    let trying = tokio::spawn({
        let applier = Arc::clone(&applier);
        async move { applier.try_one("m1").await }
    });
    replica.arrived.notified().await;
    let renewed = notes.note("m1");
    open.send(()).unwrap();
    assert_eq!(trying.await.unwrap().unwrap(), Tried::Applied);
    assert_eq!(
        notes.row("m1").map(|r| r.seq),
        Some(renewed),
        "the later note stays"
    );
    assert_eq!(replica.content(O1).as_deref(), Some("one"));
}

/// Test 5. A memory not applied is noted afresh, under a new number, its
/// try counted with why, and its next try put off.
#[tokio::test]
async fn a_memory_not_applied_is_noted_afresh_and_put_off() {
    let serving = Arc::new(Held::holding(None, vec![object(O1, "m1", "one")]));
    let replica = Arc::new(Gated::default());
    replica.down.store(true, Ordering::SeqCst);
    let notes = Arc::new(List::default());
    let first = notes.note("m1");
    let applier = applier(
        Arc::clone(&replica),
        serving,
        Arc::clone(&notes),
        Arc::new(NoTurns),
    );
    assert_eq!(applier.try_one("m1").await.unwrap(), Tried::Failed);
    let row = notes.row("m1").expect("still noted");
    assert_ne!(row.seq, first, "under a new number");
    assert_eq!(row.tries, 1);
    assert!(row.error.unwrap().contains("503"));
    assert!(row.next.unwrap() > now(), "put off");

    replica.down.store(false, Ordering::SeqCst);
    assert_eq!(
        applier.work_due(&CancellationToken::new()).await.unwrap(),
        Worked::default(),
        "not due yet"
    );
}

/// Test 6. Two appliers meet (leadership moved): the first read the older
/// content, and its write lands after the second wrote the newer. The
/// first's next pass sees they disagree and writes the newer again.
#[tokio::test]
async fn when_two_appliers_meet_the_newer_wins() {
    let serving = Arc::new(Held::holding(None, vec![object(O1, "m1", "older")]));
    let replica = Arc::new(Gated::default());
    let notes = Arc::new(List::default());
    notes.note("m1");
    let open = replica.close();
    let first = Arc::new(applier(
        Arc::clone(&replica),
        Arc::clone(&serving),
        Arc::clone(&notes),
        Arc::new(NoTurns),
    ));
    let trying = tokio::spawn({
        let first = Arc::clone(&first);
        async move { first.try_one("m1").await }
    });
    replica.arrived.notified().await;

    *serving.objects.lock().unwrap() = vec![object(O1, "m1", "newer")];
    notes.note("m1");
    let second = applier(
        Arc::clone(&replica),
        Arc::clone(&serving),
        Arc::clone(&notes),
        Arc::new(NoTurns),
    );
    assert_eq!(second.try_one("m1").await.unwrap(), Tried::Applied);
    assert_eq!(replica.content(O1).as_deref(), Some("newer"));

    open.send(()).unwrap();
    assert_eq!(trying.await.unwrap().unwrap(), Tried::Applied);
    assert_eq!(replica.content(O1).as_deref(), Some("newer"));
}

/// A source whose memory changes at every read.
struct Changing(Held, AtomicUsize);

#[async_trait]
impl SyncSource for Changing {
    async fn export_page(&self, after: Option<&str>, with_vectors: bool) -> Result<Vec<Value>> {
        self.0.export_page(after, with_vectors).await
    }
    fn embedding_model(&self) -> Option<&str> {
        None
    }
    async fn objects_of_memory(&self, memory_id: &str, _: bool) -> Result<Vec<Value>> {
        let n = self.1.fetch_add(1, Ordering::SeqCst);
        Ok(vec![object(O1, memory_id, &format!("version {n}"))])
    }
    async fn objects_by_id(&self, ids: &[String], with_vectors: bool) -> Result<Vec<Value>> {
        self.0.objects_by_id(ids, with_vectors).await
    }
}

/// Test 7. A memory that changes at every pass is given up after a bound,
/// and noted afresh: the applier never chases it for ever.
#[tokio::test]
async fn a_memory_that_never_settles_is_given_up_and_noted_afresh() {
    let serving = Arc::new(Changing(Held::default(), AtomicUsize::new(0)));
    let replica = Arc::new(Gated::default());
    let notes = Arc::new(List::default());
    notes.note("m1");
    let applier = Applier {
        replica,
        serving: Arc::clone(&serving) as Arc<dyn SyncSource>,
        notes: Arc::clone(&notes) as Arc<dyn PendingNotes>,
        turns: Arc::new(NoTurns),
        clock: Arc::new(now),
    };
    let tried = tokio::time::timeout(Duration::from_secs(5), applier.try_one("m1"))
        .await
        .expect("bounded");
    assert_eq!(tried.unwrap(), Tried::Failed);
    assert_eq!(serving.1.load(Ordering::SeqCst), SETTLE_PASSES);
    assert!(notes
        .row("m1")
        .unwrap()
        .error
        .unwrap()
        .contains("kept changing"));
}

/// A promoted replica is refused, written nothing, and the note stays.
#[tokio::test]
async fn a_promoted_replica_is_refused_and_the_note_stays() {
    let serving = Arc::new(Held::holding(None, vec![object(O1, "m1", "one")]));
    let replica = Arc::new(Gated::default());
    *replica.inner.promoted.lock().unwrap() = true;
    let notes = Arc::new(List::default());
    let seq = notes.note("m1");
    let applier = applier(
        Arc::clone(&replica),
        serving,
        Arc::clone(&notes),
        Arc::new(NoTurns),
    );
    let worked = applier.work_due(&CancellationToken::new()).await.unwrap();
    assert!(worked.refused);
    assert_eq!(notes.row("m1").unwrap().seq, seq);
    assert!(replica.inner.get(O1).is_none());
}

/// Test 12. Once the applier has worked the list, a whole replica sync
/// finds nothing left to fix.
#[tokio::test]
async fn after_the_applier_a_whole_sync_finds_nothing_to_fix() {
    let serving = Arc::new(Held::holding(
        None,
        vec![object(O1, "m1", "one, edited"), object(O2, "m2", "two")],
    ));
    let replica = Arc::new(Gated::holding(vec![
        object(O1, "m1", "one"),
        object("00000000-0000-0000-0000-000000000003", "m3", "purged"),
    ]));
    let notes = Arc::new(List::default());
    for id in ["m1", "m2", "m3"] {
        notes.note(id);
    }
    let applier = applier(
        Arc::clone(&replica),
        Arc::clone(&serving),
        Arc::clone(&notes),
        Arc::new(NoTurns),
    );
    let worked = applier.work_due(&CancellationToken::new()).await.unwrap();
    assert_eq!(worked.applied, 3);
    assert!(notes.rows.lock().unwrap().is_empty());
    let report =
        workforce_memory::admin::sync(replica.inner.as_ref(), serving.as_ref(), SyncMode::Replica)
            .await
            .unwrap();
    assert_eq!(
        report.copied + report.updated + report.dropped,
        0,
        "{report:?}"
    );
}

/// Test 3 (applier) and test 9. The applier applies on the leader alone,
/// and never while paused.
#[tokio::test(start_paused = true)]
async fn the_applier_runs_on_the_leader_alone_and_never_paused() {
    async fn applied(leader: bool, paused: bool) -> bool {
        let serving = Arc::new(Held::holding(None, vec![object(O1, "m1", "one")]));
        let replica = Arc::new(Gated::default());
        let notes = Arc::new(List::default());
        notes.note("m1");
        let drain = CancellationToken::new();
        let task = spawn_applier(
            Arc::new(applier(
                Arc::clone(&replica),
                serving,
                Arc::clone(&notes),
                Arc::new(NoTurns),
            )),
            APPLY_WATCH,
            Arc::new(AtomicBool::new(paused)),
            Some(Arc::new(AtomicBool::new(leader))),
            drain.clone(),
        );
        tokio::time::sleep(APPLY_WATCH * 3).await;
        drain.cancel();
        task.await.unwrap();
        replica.inner.get(O1).is_some()
    }
    assert!(applied(true, false).await);
    assert!(!applied(false, false).await, "a follower applies nothing");
    assert!(!applied(true, true).await, "nor a paused leader");
}

// ---- against Postgres ------------------------------------------------

/// Runs `test` against the test Postgres, the telemetry tables made, with
/// a pool whose `search_path` holds them; passes over when there is none.
async fn with_postgres<F, Fut>(test: F)
where
    F: FnOnce(workforce_common::postgres::PostgresPool) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    use workforce_common::telemetry_postgres::PostgresTelemetryStore;
    let ran = Arc::new(AtomicBool::new(false));
    let reached = Arc::clone(&ran);
    workforce_postgres::test_support::with_test_pool(
        PostgresTelemetryStore::build_pool,
        |pool| async move {
            test(pool).await;
            reached.store(true, Ordering::SeqCst);
        },
    )
    .await;
    assert!(ran.load(Ordering::SeqCst) || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok());
}

/// Test 11. The pending list's statements, as the applier relies on them.
#[tokio::test(flavor = "multi_thread")]
async fn the_pending_list_numbers_takes_and_renotes_as_the_applier_relies_on() {
    with_postgres(|pool| async move {
        let notes = MirrorNotes::with_pool(pool).await.unwrap();
        notes
            .note(&["m1".into(), "m2".into(), "m1".into()])
            .await
            .unwrap();
        let m1 = notes.seen("m1").await.unwrap().unwrap();
        assert_eq!(m1.tries, 0);

        // A failed try renotes under a new number, counted and put off.
        let at = Utc::now();
        notes
            .renoted(
                "m1",
                m1.seq,
                "vellev answered 503",
                at,
                at + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        let failed = notes.seen("m1").await.unwrap().unwrap();
        assert!(failed.seq > m1.seq);
        assert_eq!(failed.tries, 1);
        assert_eq!(
            ids(notes.due(at, None, 10).await.unwrap()),
            vec!["m2".to_string()]
        );
        // A renote at a number no longer current counts no try.
        notes.renoted("m1", m1.seq, "late", at, at).await.unwrap();
        assert_eq!(notes.seen("m1").await.unwrap().unwrap().tries, 1);

        // A write noted again: tries forgotten, due at once, never-failed first.
        notes.note(&["m1".into()]).await.unwrap();
        let again = notes.seen("m1").await.unwrap().unwrap();
        assert_eq!(again.tries, 0);
        assert_eq!(notes.due(at, None, 10).await.unwrap().len(), 2);

        // Taken only at its own number.
        assert!(!notes.taken("m1", failed.seq).await.unwrap());
        assert!(notes.taken("m1", again.seq).await.unwrap());
        assert!(notes.seen("m1").await.unwrap().is_none());

        let backlog = notes.backlog().await.unwrap();
        assert_eq!(backlog.pending, 1);
        assert!(backlog.oldest.is_some());
    })
    .await;
}

fn ids(due: Vec<DueNote>) -> Vec<String> {
    due.into_iter().map(|d| d.id).collect()
}

/// Review finding 4. A round pages through the list by a cursor, so what
/// it asks stays the same size: every due note is given once, never-failed
/// first, then by when it is due; and a note put off during the round, or
/// one not yet due, is not given.
#[tokio::test(flavor = "multi_thread")]
async fn the_due_notes_are_paged_by_a_cursor_each_once_in_order() {
    with_postgres(|pool| async move {
        let notes = MirrorNotes::with_pool(pool).await.unwrap();
        let fresh: Vec<String> = (0..5).map(|n| format!("f{n}")).collect();
        let failed: Vec<String> = (0..4).map(|n| format!("x{n}")).collect();
        notes.note(&fresh).await.unwrap();
        notes.note(&failed).await.unwrap();
        notes.note(&["later".into()]).await.unwrap();
        let now = Utc::now();
        // Failed earlier, due at different times in the past, x3 and x0 at
        // the same time (the id breaks it).
        for (id, ago) in [("x0", 30), ("x1", 50), ("x2", 40), ("x3", 30)] {
            let seq = notes.seen(id).await.unwrap().unwrap().seq;
            let at = now - chrono::Duration::seconds(ago);
            notes.renoted(id, seq, "e", at, at).await.unwrap();
        }
        let seq = notes.seen("later").await.unwrap().unwrap().seq;
        notes
            .renoted("later", seq, "e", now, now + chrono::Duration::hours(1))
            .await
            .unwrap();

        let mut given = Vec::new();
        let mut after: Option<DueNote> = None;
        loop {
            let page = notes.due(now, after.as_ref(), 2).await.unwrap();
            if page.is_empty() {
                break;
            }
            // f0 fails during the round, put off past the round's `now`:
            // not given again in it.
            if after.is_none() {
                let seq = notes.seen("f0").await.unwrap().unwrap().seq;
                let next = now + chrono::Duration::seconds(2);
                notes.renoted("f0", seq, "e", now, next).await.unwrap();
            }
            after = page.last().cloned();
            given.extend(ids(page));
        }
        assert_eq!(
            given,
            ["f0", "f1", "f2", "f3", "f4", "x1", "x2", "x0", "x3"]
                .map(String::from)
                .to_vec()
        );
    })
    .await;
}

/// The list as it is while every memory is written again during each try
/// of it: its note is renewed between the applier's read of it and its
/// taking it away, so it stays, due at once.
/// It refuses past `pages` asks of what is due, so a round that never
/// ends fails rather than spinning (it never yields, so no timeout could
/// stop it).
struct Rewritten(Arc<List>, AtomicUsize);

#[async_trait]
impl PendingNotes for Rewritten {
    async fn due(
        &self,
        now: DateTime<Utc>,
        after: Option<&DueNote>,
        limit: i64,
    ) -> Result<Vec<DueNote>> {
        if self.1.fetch_add(1, Ordering::SeqCst) > 10 {
            return Err(eyre::eyre!("asked for what is due more than ten times"));
        }
        self.0.due(now, after, limit).await
    }
    async fn seen(&self, id: &str) -> Result<Option<Noted>> {
        let seen = self.0.seen(id).await?;
        self.0.note(id);
        Ok(seen)
    }
    async fn taken(&self, id: &str, seq: i64) -> Result<bool> {
        self.0.taken(id, seq).await
    }
    async fn renoted(
        &self,
        id: &str,
        seq: i64,
        error: &str,
        now: DateTime<Utc>,
        next: DateTime<Utc>,
    ) -> Result<()> {
        self.0.renoted(id, seq, error, now, next).await
    }
}

/// Review finding 4, the applier's side. A round goes on from where it
/// reached, so it tries each memory once and ends, even when every note
/// stays due at once (each written again while it is applied), over more
/// than a page.
#[tokio::test]
async fn a_round_tries_each_memory_once_and_ends() {
    let serving = Arc::new(Held::holding(None, vec![]));
    let replica = Arc::new(Gated::default());
    let list = Arc::new(List::default());
    let n = APPLY_PAGE as usize + 5;
    for i in 0..n {
        list.note(&format!("m{i:04}"));
    }
    let applier = Applier {
        replica,
        serving,
        notes: Arc::new(Rewritten(Arc::clone(&list), AtomicUsize::new(0))),
        turns: Arc::new(NoTurns),
        clock: Arc::new(now),
    };
    let worked = applier
        .work_due(&CancellationToken::new())
        .await
        .expect("the round ended within its pages");
    assert_eq!(worked.applied, n, "each tried once");
    assert_eq!(list.rows.lock().unwrap().len(), n, "every later note stays");
}

/// Test 10 (and 10b). The applier holds the replica's lock shared for each
/// pass, and an erasure, holding it alone, waits for the pass in hand: no
/// write of the replica decided on a read made before the erasure lands
/// after it. Here the applier has read memory X from Weaviate and its
/// write to vellev is in flight when the erasure starts.
#[tokio::test(flavor = "multi_thread")]
async fn an_erasure_waits_for_the_applier_s_write_in_hand() {
    with_postgres(|pool| async move {
        use workforce_common::shadow_postgres::{ReplicaLock, ShadowStore};
        ShadowStore::with_pool(pool.clone()).await.unwrap();
        let lock = Arc::new(ReplicaLock::new(pool));
        let serving = Arc::new(Held::holding(None, vec![object(O1, "x", "about bob")]));
        let replica = Arc::new(Gated::default());
        let notes = Arc::new(List::default());
        notes.note("x");
        let open = replica.close();
        let turns = applier_turns(Arc::clone(&lock));
        let applier = Arc::new(applier(
            Arc::clone(&replica),
            Arc::clone(&serving),
            Arc::clone(&notes),
            turns,
        ));
        let trying = tokio::spawn({
            let applier = Arc::clone(&applier);
            async move { applier.try_one("x").await }
        });
        replica.arrived.notified().await;

        // The erasure: the lock alone, the replica, then Weaviate.
        let erasing = tokio::spawn({
            let (lock, serving, replica) = (
                Arc::clone(&lock),
                Arc::clone(&serving),
                Arc::clone(&replica),
            );
            async move {
                let held = lock
                    .exclusive(Duration::from_secs(10), Duration::from_secs(60))
                    .await
                    .unwrap();
                replica.inner.delete(O1).await.unwrap();
                serving.delete(O1).await.unwrap();
                replica.events.lock().unwrap().push("erased".into());
                held.release().await.unwrap();
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        open.send(()).unwrap();
        erasing.await.unwrap();
        trying.await.unwrap().unwrap();

        let events = replica.events.lock().unwrap().clone();
        let erased = events.iter().position(|e| e == "erased").unwrap();
        assert!(
            events[erased..].iter().all(|e| !e.ends_with(O1)),
            "nothing of X written after its erasure: {events:?}"
        );
        assert!(replica.inner.get(O1).is_none(), "{events:?}");
    })
    .await;
}

// ---- against a vellev node -------------------------------------------

/// Embeds as a hub model named `hub`.
struct Hub;

#[async_trait]
impl workforce_memory::embedding::EmbeddingBackend for Hub {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(vec![text.len() as f32, 1.0])
    }
    fn model_id(&self) -> &str {
        "hub"
    }
}

/// Against the node `VELLEV_TEST_URL` and `VELLEV_TEST_KEY` name, in a
/// fresh tenant; skipped without them. A noted memory reaches vellev under
/// Weaviate's object id (design r2 section 4.1), and once Weaviate no
/// longer holds it, leaves.
#[tokio::test]
async fn a_noted_memory_reaches_the_node_under_weaviate_s_object_id_and_leaves() {
    let Some(node) = workforce_vellev::testing::Node::fresh().await else {
        return;
    };
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.memory.backend = workforce_common::settings::SHADOWED.into();
    config.vellev.url = node.url.clone();
    config.vellev.api_key = Some(node.key.clone());
    config.vellev.tenant = node.tenant.clone();
    config.weaviate.enabled = true;
    let (replica, _) = crate::embeddings::paired(&config, Arc::new(Hub), None).unwrap();
    replica.ensure_collection().await.unwrap();
    let mut held = object(O1, "m1", "about the node");
    held["vector"] = serde_json::json!([14.0, 1.0]);
    let serving = Arc::new(Held::holding(Some("hub"), vec![held]));
    let notes = Arc::new(List::default());
    notes.note("m1");
    let applier = Applier {
        replica: Arc::clone(&replica) as Arc<dyn SyncTarget>,
        serving: Arc::clone(&serving) as Arc<dyn SyncSource>,
        notes: Arc::clone(&notes) as Arc<dyn PendingNotes>,
        turns: Arc::new(NoTurns),
        clock: Arc::new(now),
    };
    assert_eq!(applier.try_one("m1").await.unwrap(), Tried::Applied);
    let there = replica.objects_of_memory("m1", false).await.unwrap();
    assert_eq!(there.len(), 1);
    assert_eq!(there[0]["id"], O1);

    serving.objects.lock().unwrap().clear();
    notes.note("m1");
    assert_eq!(applier.try_one("m1").await.unwrap(), Tried::Applied);
    assert!(replica
        .objects_of_memory("m1", false)
        .await
        .unwrap()
        .is_empty());
}

/// The pending list's pool is made ready, its migrations run, within a
/// bound: a migration run holds one connection for its lock and asks for
/// another, so a pool of one would wait for ever, and the applier, which
/// waits for it, would never start.
#[tokio::test(flavor = "multi_thread")]
async fn the_pending_list_s_own_pool_is_made_ready() {
    let pg: Arc<Mutex<Option<workforce_common::postgres::PostgresConfig>>> = Arc::default();
    let seen = Arc::clone(&pg);
    let ran = Arc::new(AtomicBool::new(false));
    let reached = Arc::clone(&ran);
    workforce_postgres::test_support::with_test_pool(
        move |cfg| {
            *seen.lock().unwrap() = Some(cfg.clone());
            workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool(cfg)
        },
        |_| async move {
            let mut config = workforce_common::settings::WorkforceConfig::default();
            config.postgres = pg.lock().unwrap().clone();
            let notes = tokio::time::timeout(Duration::from_secs(20), notes(&config))
                .await
                .expect("ready within 20 s")
                .unwrap();
            notes.note(&["m1".into()]).await.unwrap();
            reached.store(true, Ordering::SeqCst);
        },
    )
    .await;
    assert!(ran.load(Ordering::SeqCst) || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok());
}
