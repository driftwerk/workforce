//! Setting up, checking and moving a memory store's contents, whichever
//! database holds them.
//!
//! Memories move between stores as raw objects, `{ id, class, properties,
//! vector }`, the shape of the backup files too. A store keeps the ids it
//! is given, so copying is idempotent and safe from several replicas at
//! once, and property names are the same in every store, so properties
//! compare directly.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use futures::StreamExt;
use serde_json::Value;
use tracing::{info, warn};
use workforce_common::vectorizable::VectorizableText;

use crate::store::MemoryStore;

/// Objects per import request, where the store need not embed them.
pub const IMPORT_BATCH_SIZE: usize = 100;

/// What a schema rebuild did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    AlreadyValid,
    Rebuilt {
        exported: usize,
        imported: usize,
        backup_path: Option<PathBuf>,
        mismatches: Vec<String>,
    },
}

/// How [`sync`] treats memories both stores hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Before the switch, while the source is the store in use: copy,
    /// update and drop whatever the target holds that the source no longer
    /// does.
    Mirror,
    /// After the switch, while something still writes to the source: copy
    /// only new memories and carry over forgetting, never anything that
    /// would undo a change made in the target.
    CatchUp,
    /// Back into the previous store, once nothing writes to it any more:
    /// as a catch-up, and drop what it holds made since the switch that
    /// the source no longer does, so that a memory erased in the source
    /// while it was being carried back does not stay.
    Back { since: DateTime<Utc> },
    /// While the source is the store in use and the target only mirrors
    /// it, as vellev does in shadow mode (DW-676): as a mirror, but never
    /// turned into a catch-up by the switch marker, which an import copies
    /// along with the memories. Refused once the target is promoted (see
    /// [`SyncTarget::promoted`]): memories are then written there, and a
    /// replica would drop each written since.
    Replica,
    /// Back into the store memories were promoted from, once the target
    /// they were promoted to is to be given up (DW-676, `workforce shadow
    /// rollback`): as a replica, carrying every change made since the
    /// promotion, new memories, changed ones and those erased, and never
    /// turned into a catch-up by the target's switch marker. The target is
    /// not asked whether it is promoted, Weaviate refusing the question;
    /// the caller has checked that the source is.
    Rollback,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub copied: usize,
    pub updated: usize,
    pub dropped: usize,
    pub unchanged: usize,
}

/// What a sync did, and to which objects: for a reconcile, what it had
/// to fix, each to be traced to how it was missed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Synced {
    pub report: SyncReport,
    /// Each object the sync wrote to or dropped from the target.
    pub fixed: Vec<Fix>,
}

/// One object a sync wrote to or dropped from its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub kind: FixKind,
    pub object_id: String,
    /// The memory it holds, when the object says.
    pub memory_id: Option<String>,
}

/// What a sync did to an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixKind {
    /// Stored, new or again.
    Copied,
    /// Its properties changed in place.
    Updated,
    Dropped,
}

impl FixKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copied => "copied",
            Self::Updated => "updated",
            Self::Dropped => "dropped",
        }
    }
}

/// The operator's view of a memory store: its collection and its contents
/// as a whole.
#[async_trait]
pub trait MemoryAdmin: Send + Sync {
    /// The collection memories are stored in.
    fn collection(&self) -> &str;

    /// Create the collection, or check an existing one; `Err` when it has
    /// a shape that cannot be used.
    async fn ensure_collection(&self) -> Result<()>;

    /// As [`Self::ensure_collection`], rebuilding a collection whose shape
    /// is wrong from an export of it, written to `backup_dir` first.
    async fn ensure_collection_with_recovery(
        &self,
        backup_dir: Option<&Path>,
    ) -> Result<RecoveryOutcome>;

    /// Check the collection's shape without changing anything.
    async fn doctor(&self) -> Result<()>;

    /// Store every object of an export, keeping its id and, where it has
    /// one, its vector; otherwise the store embeds it. Returns how many
    /// were stored.
    async fn import(&self, objects: &[Value]) -> Result<usize>;

    /// Replace the tags of the object with this id.
    async fn patch_tags(&self, id: &str, tags: &[String]) -> Result<()>;

    /// Delete the collection and everything in it.
    async fn delete_collection(&self) -> Result<()>;
}

/// Objects per page of an export.
pub const EXPORT_PAGE: usize = 100;

/// A store memories can be copied from.
#[async_trait]
pub trait SyncSource: Send + Sync {
    /// Up to [`EXPORT_PAGE`] objects whose ids come after `after`, in
    /// ascending order of their ids as strings (the order of lowercase
    /// UUIDs as bytes), with their vectors when asked for. A page shorter
    /// than [`EXPORT_PAGE`] is the last.
    async fn export_page(&self, after: Option<&str>, with_vectors: bool) -> Result<Vec<Value>>;

    /// Every object, with its vector when asked for, held in memory at
    /// once: for a whole collection that must be held anyway, as when it
    /// is rebuilt. A sync reads a page at a time instead.
    async fn export(&self, with_vectors: bool) -> Result<Vec<Value>> {
        let mut cursor = Cursor::new(self, with_vectors);
        let mut all = Vec::new();
        while let Some(object) = cursor.next().await? {
            all.push(object);
        }
        Ok(all)
    }

    /// The model whose vectors the store holds, when the store knows it:
    /// the hub's, or the one a store that embeds with a known model runs
    /// (vellev's bge); `None` when the database's own vectorizer makes them
    /// (Weaviate's). Vectors are copied only between stores naming the same.
    fn embedding_model(&self) -> Option<&str>;

    /// Every object holding the memory `memory_id`, with its vector when
    /// asked for, as [`Self::export_page`] gives them: a memory can be held
    /// by more than one object.
    async fn objects_of_memory(&self, memory_id: &str, with_vectors: bool) -> Result<Vec<Value>>;

    /// The objects with these ids, each with its vector when asked for, as
    /// [`Self::export_page`] gives them; an id held by none is left out.
    async fn objects_by_id(&self, ids: &[String], with_vectors: bool) -> Result<Vec<Value>>;
}

/// Turns at writing into a replica's target. A sync takes one around each
/// page it reads, decides and writes, and gives it up after, so that what
/// must not overlap those writes (an erasure under way, which a copy read
/// before it would undo) waits for at most the page in hand, and keeps the
/// next from starting until it is done.
#[async_trait]
pub trait WriteTurns: Send + Sync {
    /// Wait for a turn; `Err` when none can be had, and the sync stops.
    async fn take(&self) -> Result<Box<dyn Turn>>;
}

/// One turn of [`WriteTurns`], held until [`Turn::done`].
#[async_trait]
pub trait Turn: Send {
    async fn done(self: Box<Self>) -> Result<()>;
}

/// Turns always free: for a sync nothing else must wait on.
pub struct NoTurns;

#[async_trait]
impl WriteTurns for NoTurns {
    async fn take(&self) -> Result<Box<dyn Turn>> {
        Ok(Box::new(NoTurns))
    }
}

#[async_trait]
impl Turn for NoTurns {
    async fn done(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// A store memories can be copied into.
#[async_trait]
pub trait SyncTarget: SyncSource {
    /// As [`MemoryAdmin::import`].
    async fn upsert(&self, objects: &[Value]) -> Result<usize>;

    /// Merge `properties` into the object with this id, keeping its vector.
    async fn patch(&self, id: &str, properties: Value) -> Result<()>;

    /// Remove the object with this id for good.
    async fn delete(&self, id: &str) -> Result<()>;

    /// Vectors for `texts` as this store stores them, or `None` when the
    /// store embeds for itself.
    async fn vectors(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>>;

    /// Whether memories have switched to this store: a marker written once
    /// a full sync finished.
    async fn switched(&self) -> Result<bool>;

    /// Record the switch; idempotent.
    async fn mark_switched(&self) -> Result<()>;

    /// Whether memories have been promoted to this store from the one it
    /// was a replica of: a marker written only by promotion, never by an
    /// import or a switch.
    async fn promoted(&self) -> Result<bool>;
}

/// A store that can take over from another and hand back to it.
pub trait Switchable: MemoryStore + MemoryAdmin + SyncTarget {
    /// Forget in `previous` too from now on, so that rolling back never
    /// brings a forgotten memory back.
    fn keep_previous(&self, previous: Arc<dyn MemoryStore>);
}

/// Collection that records which hub-vectored collections are in use, and
/// the marker's id in it: the same in Weaviate and vellev.
pub(crate) use workforce_vellev::{marker_id as switch_marker_id, MARKERS as SWITCH_COLLECTION};

/// Bring `target` in line with `source`, which is never written. Memories
/// whose content changed are embedded again; other changes are patched
/// onto the object, keeping its vector. Vectors are copied rather than
/// made again when both stores hold the same model's.
///
/// A mirror that finds another replica has already switched (and may be
/// writing to the target) runs as a catch-up instead. A replica marks the
/// switch before it writes there, so a memory it wrote that this export
/// saw is never overwritten or dropped.
pub async fn sync(
    target: &dyn SyncTarget,
    source: &dyn SyncSource,
    mode: SyncMode,
) -> Result<SyncReport> {
    sync_with(target, source, mode, &NoTurns).await
}

/// As [`sync`], taking a turn of `turns` around each page: what a page
/// writes is decided only on what was read within its turn, and nothing
/// is read or written between turns.
///
/// A replica walks the source without vectors, the source being the
/// store in use, and reads afresh, with its vector, each object it is to
/// copy.
pub async fn sync_with(
    target: &dyn SyncTarget,
    source: &dyn SyncSource,
    mode: SyncMode,
    turns: &dyn WriteTurns,
) -> Result<SyncReport> {
    let mut synced = Synced::default();
    sync_recorded(target, source, mode, turns, &mut synced).await?;
    Ok(synced.report)
}

/// As [`sync_with`], keeping in `into` what it did, each object by name,
/// as it goes: a sync that fails part way leaves there what it did before.
pub async fn sync_recorded(
    target: &dyn SyncTarget,
    source: &dyn SyncSource,
    mut mode: SyncMode,
    turns: &dyn WriteTurns,
    into: &mut Synced,
) -> Result<()> {
    let copying_vectors =
        source.embedding_model().is_some() && source.embedding_model() == target.embedding_model();
    if mode == SyncMode::Replica {
        refuse_promoted(target).await?;
    }
    if mode == SyncMode::Mirror && target.switched().await? {
        mode = SyncMode::CatchUp;
    }
    // Where the hub makes the target's vectors, a memory it holds without
    // one is one whose embedding failed when it was written: read with its
    // vector, to find those.
    let vectors_due = target.vectors(&[]).await?.is_some();
    let lean = matches!(mode, SyncMode::Replica | SyncMode::Rollback) && copying_vectors;
    let mut walk = Walk {
        target,
        source,
        ours: Cursor::new(target, vectors_due),
        theirs: Cursor::new(source, copying_vectors && !lean).normalised(),
        mode,
        vectors_due,
        copying_vectors,
        lean,
        synced: std::mem::take(into),
    };
    let walked = async {
        loop {
            let turn = turns.take().await?;
            let walked = walk.page().await;
            let released = turn.done().await;
            let more = walked?;
            released?;
            if !more {
                return eyre::Ok(());
            }
        }
    }
    .await;
    *into = std::mem::take(&mut walk.synced);
    walked?;
    let report = into.report;
    info!(
        target: "embeddings",
        mode = ?walk.mode,
        copied = report.copied,
        updated = report.updated,
        dropped = report.dropped,
        unchanged = report.unchanged,
        "memory sync complete"
    );
    Ok(())
}

/// A sync under way: both stores walked in step, a page at a time.
struct Walk<'a> {
    target: &'a (dyn SyncTarget + 'a),
    source: &'a (dyn SyncSource + 'a),
    ours: Cursor<'a, dyn SyncTarget + 'a>,
    theirs: Cursor<'a, dyn SyncSource + 'a>,
    mode: SyncMode,
    vectors_due: bool,
    copying_vectors: bool,
    /// The source is walked without vectors; those copied are read again.
    lean: bool,
    synced: Synced,
}

impl Walk<'_> {
    /// Decide on up to a page of objects and write what they need; whether
    /// any are left. What a turn before this one read and did not decide
    /// on is read again: it may have changed, or been erased, since.
    async fn page(&mut self) -> Result<bool> {
        // Every id up to the last given out by either side is decided on
        // in both stores; what comes after is read again from there, both
        // sides alike (a copy just written lies after the target's last).
        let decided = self.ours.given.clone().max(self.theirs.given.clone());
        self.ours.rewind_to(decided.clone());
        self.theirs.rewind_to(decided);
        let mut pending = Pending::default();
        let mut more = true;
        for _ in 0..EXPORT_PAGE {
            let order = match (self.ours.peek_id().await?, self.theirs.peek_id().await?) {
                (None, None) => {
                    more = false;
                    break;
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(mine), Some(other)) => mine.cmp(other),
            };
            match order {
                Ordering::Less => {
                    let object = self.ours.next().await?.expect("peeked");
                    let dropped = match self.mode {
                        SyncMode::Mirror | SyncMode::Replica | SyncMode::Rollback => true,
                        SyncMode::CatchUp => false,
                        SyncMode::Back { since } => created(&object).is_some_and(|at| at >= since),
                    };
                    if dropped {
                        pending.note(&object);
                        pending.drops.push(id_of(&object));
                    }
                }
                Ordering::Greater => {
                    let object = self.theirs.next().await?.expect("peeked");
                    pending.note(&object);
                    pending.copies.push(object);
                }
                Ordering::Equal => {
                    let held = self.ours.next().await?.expect("peeked");
                    let object = self.theirs.next().await?.expect("peeked");
                    pending.note(&object);
                    let step = step(self.mode, &held, object, self.vectors_due);
                    pending.take(step, &mut self.synced.report);
                }
            }
        }
        self.mode = pending.checked(self.target, self.mode).await?;
        if self.lean {
            pending.read_again(self.source).await?;
        }
        pending
            .flush(self.target, self.copying_vectors, &mut self.synced)
            .await?;
        Ok(more)
    }
}

/// Bring what `target` holds of the memory `memory_id` in line with what
/// `source` holds of it, as a [`SyncMode::Replica`] of the whole store
/// would: each object of the source's holding it is copied under its own
/// id, or updated, and every object of the target's holding it that the
/// source does not hold is dropped, so a memory the source no longer
/// holds at all is taken from the target whole. Refused, as a replica is,
/// once the target is promoted.
pub async fn sync_memory(
    target: &dyn SyncTarget,
    source: &dyn SyncSource,
    memory_id: &str,
    turns: &dyn WriteTurns,
) -> Result<SyncReport> {
    let turn = turns.take().await?;
    let synced = sync_one(target, source, memory_id).await;
    let released = turn.done().await;
    let report = synced?;
    released?;
    Ok(report)
}

async fn sync_one(
    target: &dyn SyncTarget,
    source: &dyn SyncSource,
    memory_id: &str,
) -> Result<SyncReport> {
    refuse_promoted(target).await?;
    let copying_vectors =
        source.embedding_model().is_some() && source.embedding_model() == target.embedding_model();
    let vectors_due = target.vectors(&[]).await?.is_some();
    // As a whole sync walks them: objects without an id, or without text
    // to embed, are passed over in the source, and so dropped from the
    // target.
    let theirs: Vec<Value> = source
        .objects_of_memory(memory_id, copying_vectors)
        .await?
        .iter()
        .filter(|object| object["id"].is_string())
        .filter_map(normalised)
        .collect();
    let ours: Vec<Value> = target
        .objects_of_memory(memory_id, vectors_due)
        .await?
        .into_iter()
        .filter(|object| object["id"].is_string())
        .collect();
    let mut pending = Pending::default();
    let mut synced = Synced::default();
    for held in &ours {
        if !theirs.iter().any(|object| object["id"] == held["id"]) {
            pending.note(held);
            pending.drops.push(id_of(held));
        }
    }
    for object in theirs {
        pending.note(&object);
        match ours.iter().find(|held| held["id"] == object["id"]) {
            Some(held) => pending.take(
                step(SyncMode::Replica, held, object, vectors_due),
                &mut synced.report,
            ),
            None => pending.copies.push(object),
        }
    }
    pending.checked(target, SyncMode::Replica).await?;
    pending.flush(target, copying_vectors, &mut synced).await?;
    Ok(synced.report)
}

/// `Err` ([`Promoted`]) if `target` has been promoted.
async fn refuse_promoted(target: &dyn SyncTarget) -> Result<()> {
    match target.promoted().await? {
        true => Err(eyre::Report::new(Promoted)),
        false => Ok(()),
    }
}

/// What a sync does with an object both stores hold.
enum Step {
    Unchanged,
    Copy(Value),
    Patch(String, Value),
}

/// What a sync in `mode` does with `object`, held in the target as `held`.
/// `vectors_due`: the hub makes the target's vectors.
fn step(mode: SyncMode, held: &Value, object: Value, vectors_due: bool) -> Step {
    // Held with no vector where the hub makes them: embedded now, whatever
    // else is so. A mirror or a replica takes the source's memory;
    // otherwise the target's own, which may be newer.
    let unvectored = vectors_due && !held["vector"].is_array();
    let mine = &held["properties"];
    match mode {
        SyncMode::Mirror | SyncMode::Replica | SyncMode::Rollback => {
            let other = &object["properties"];
            if unvectored {
                Step::Copy(object)
            } else if same_properties(mine, other) {
                Step::Unchanged
            } else if mine["content"] != other["content"] {
                // Other content is another vector: stored again, not
                // patched, which would keep the vector of the old.
                Step::Copy(object)
            } else {
                Step::Patch(id_of(&object), patch_of(mine, other))
            }
        }
        SyncMode::CatchUp | SyncMode::Back { .. } => {
            let other = &object["properties"];
            if unvectored {
                Step::Copy(held.clone())
            } else if other["deleted"] == true && mine["deleted"] != true {
                Step::Patch(id_of(&object), deleted())
            } else {
                Step::Unchanged
            }
        }
    }
}

/// What a [`SyncMode::Replica`] sync fails with when its target has been
/// promoted: memories are written there now, and a replica would drop
/// every one written since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Promoted;

impl std::fmt::Display for Promoted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "memories have been promoted to the replica's store: a replica sync would drop every memory written there since, so none is run"
        )
    }
}

impl std::error::Error for Promoted {}

/// What a sync has yet to write, written a batch at a time.
#[derive(Default)]
struct Pending {
    copies: Vec<Value>,
    patches: Vec<(String, Value)>,
    drops: Vec<String>,
    /// The memory each object decided on holds, by the object's id.
    memories: std::collections::HashMap<String, Option<String>>,
}

impl Pending {
    /// `mode`, unless a mirror finds another replica switched to the target
    /// since it began, and may be writing there: then what the mirror would
    /// have dropped or changed is left be, and it goes on as a catch-up. A
    /// replica that finds its target promoted since it began writes
    /// nothing more, and fails ([`Promoted`]).
    async fn checked(&mut self, target: &dyn SyncTarget, mode: SyncMode) -> Result<SyncMode> {
        if mode == SyncMode::Replica {
            refuse_promoted(target).await?;
            return Ok(mode);
        }
        if mode != SyncMode::Mirror || !target.switched().await? {
            return Ok(mode);
        }
        self.drops.clear();
        self.patches
            .retain(|(_, properties)| *properties == deleted());
        Ok(SyncMode::CatchUp)
    }

    /// The objects to be copied read again, whole and with their vectors:
    /// one gone since the walk read it is not copied.
    async fn read_again(&mut self, source: &dyn SyncSource) -> Result<()> {
        if self.copies.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = self.copies.iter().map(id_of).collect();
        self.copies = source
            .objects_by_id(&ids, true)
            .await?
            .iter()
            .filter(|object| object["id"].is_string())
            .filter_map(normalised)
            .collect();
        Ok(())
    }

    /// Keep which memory `object` holds, for what is done to it.
    fn note(&mut self, object: &Value) {
        let memory = object["properties"]["memoryId"].as_str().map(String::from);
        self.memories.insert(id_of(object), memory);
    }

    fn fix(&self, kind: FixKind, object_id: String) -> Fix {
        let memory_id = self.memories.get(&object_id).cloned().flatten();
        Fix {
            kind,
            object_id,
            memory_id,
        }
    }

    fn take(&mut self, step: Step, report: &mut SyncReport) {
        match step {
            Step::Unchanged => report.unchanged += 1,
            Step::Copy(object) => self.copies.push(object),
            Step::Patch(id, properties) => self.patches.push((id, properties)),
        }
    }

    async fn flush(
        &mut self,
        target: &dyn SyncTarget,
        copying_vectors: bool,
        synced: &mut Synced,
    ) -> Result<()> {
        let (with, without): (Vec<Value>, Vec<Value>) = std::mem::take(&mut self.copies)
            .into_iter()
            .partition(|object| copying_vectors && object["vector"].is_array());
        // Every object a store can hold is stored, or the sync fails:
        // what it did not store, a switch must not be recorded over.
        let stored = |sent: Vec<Value>| async move {
            let due = sent.iter().filter(|o| normalised(o).is_some()).count();
            let stored = target.upsert(&sent).await?;
            if stored < due {
                return Err(eyre::eyre!(
                    "the store took {stored} of {due} memories it was sent, and did not say which it left out"
                ));
            }
            Ok(stored)
        };
        // Those without a vector the target can take are given the
        // target's (or none, where it embeds for itself).
        for (sent, to_vector) in [(with, false), (without, true)] {
            if sent.is_empty() {
                continue;
            }
            let ids: Vec<String> = sent.iter().map(id_of).collect();
            let sent = match to_vector {
                true => vectored(target, &sent).await?,
                false => sent,
            };
            match stored(sent).await {
                Ok(stored) => {
                    synced.report.copied += stored;
                    for id in ids {
                        synced.fixed.push(self.fix(FixKind::Copied, id));
                    }
                }
                Err(e) => {
                    // What the store says it did take is kept, and fixed.
                    if let Some(short) = e.downcast_ref::<ImportShort>() {
                        synced.report.copied += short.stored;
                        let failed: Vec<&String> = short.failed.iter().map(|(id, _)| id).collect();
                        for id in ids.into_iter().filter(|id| !failed.contains(&id)) {
                            synced.fixed.push(self.fix(FixKind::Copied, id));
                        }
                    }
                    return Err(e);
                }
            }
        }
        let patched: Vec<(String, Result<()>)> =
            futures::stream::iter(std::mem::take(&mut self.patches))
                .map(|(id, properties)| async move {
                    let patched = target.patch(&id, properties).await;
                    (id, patched)
                })
                .buffer_unordered(8)
                .collect()
                .await;
        // What was patched is kept whether or not another patch failed.
        let mut failed = None;
        for (id, result) in patched {
            match result {
                Ok(()) => {
                    synced.report.updated += 1;
                    synced.fixed.push(self.fix(FixKind::Updated, id));
                }
                Err(e) => failed = failed.or(Some(e)),
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        for id in std::mem::take(&mut self.drops) {
            target.delete(&id).await?;
            synced.report.dropped += 1;
            synced.fixed.push(self.fix(FixKind::Dropped, id));
        }
        Ok(())
    }
}

/// A store's objects in id order, read a page at a time. Objects without
/// an id are passed over, and a store that gives its objects out of order
/// is refused: a sync matches the two stores' objects by walking both in
/// step.
pub struct Cursor<'a, S: SyncSource + ?Sized> {
    source: &'a S,
    with_vectors: bool,
    normalised: bool,
    page: VecDeque<Value>,
    after: Option<String>,
    /// The id of the last object given out.
    given: Option<String>,
    done: bool,
}

impl<'a, S: SyncSource + ?Sized> Cursor<'a, S> {
    pub fn new(source: &'a S, with_vectors: bool) -> Self {
        Self {
            source,
            with_vectors,
            normalised: false,
            page: VecDeque::new(),
            after: None,
            given: None,
            done: false,
        }
    }

    /// Forget what was read, to read again what comes after `decided`.
    fn rewind_to(&mut self, decided: Option<String>) {
        self.page.clear();
        self.after = decided.clone();
        self.given = decided;
        self.done = false;
    }

    /// Give out objects as [`normalised`] makes them, passing over those
    /// it refuses.
    fn normalised(self) -> Self {
        Self {
            normalised: true,
            ..self
        }
    }

    async fn filled(&mut self) -> Result<()> {
        while self.page.is_empty() && !self.done {
            let page = self
                .source
                .export_page(self.after.as_deref(), self.with_vectors)
                .await?;
            self.done = page.len() < EXPORT_PAGE;
            for object in page {
                let Some(id) = object["id"].as_str().map(String::from) else {
                    warn!(?object, "an object without an id is passed over");
                    continue;
                };
                if self.after.as_ref().is_some_and(|after| id <= *after) {
                    return Err(eyre::eyre!(
                        "a store gave {id} after {}: its objects are not in id order",
                        self.after.as_deref().unwrap_or_default()
                    ));
                }
                self.after = Some(id);
                match self.normalised {
                    true => self.page.extend(normalised(&object)),
                    false => self.page.push_back(object),
                }
            }
        }
        Ok(())
    }

    /// The id of the next object, if there is one.
    async fn peek_id(&mut self) -> Result<Option<&str>> {
        self.filled().await?;
        Ok(self.page.front().and_then(|object| object["id"].as_str()))
    }

    /// The next object, if there is one.
    pub async fn next(&mut self) -> Result<Option<Value>> {
        self.filled().await?;
        let next = self.page.pop_front();
        if let Some(id) = next.as_ref().and_then(|object| object["id"].as_str()) {
            self.given = Some(id.to_string());
        }
        Ok(next)
    }
}

/// What an import fails with when it stored fewer objects than it was to:
/// how many it did store, and each object it could not, with why. The
/// objects that could be stored are stored: one that cannot be does not
/// take its batch with it. An import that fails so has not brought its
/// store in line with the source, and whoever asked for it must not go on
/// as if it had: a sync that fails never records a switch.
#[derive(Debug)]
pub struct ImportShort {
    pub stored: usize,
    /// `(id, why)` of each object that could not be stored.
    pub failed: Vec<(String, String)>,
}

impl std::fmt::Display for ImportShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} of {} objects could not be stored",
            self.failed.len(),
            self.stored + self.failed.len()
        )?;
        for (id, why) in self.failed.iter().take(10) {
            write!(f, "; {id}: {why}")?;
        }
        if self.failed.len() > 10 {
            write!(f, "; and {} more", self.failed.len() - 10)?;
        }
        Ok(())
    }
}

impl std::error::Error for ImportShort {}

/// `Ok(stored)`, or [`ImportShort`] if anything could not be stored.
pub(crate) fn imported(stored: usize, failed: Vec<(String, String)>) -> Result<usize> {
    match failed.is_empty() {
        true => Ok(stored),
        false => Err(eyre::Report::new(ImportShort { stored, failed })),
    }
}

/// `object` as a store is to hold it: with an `identityId` of `""`, as old
/// rows have, made null, so that a memory is scoped to no person rather
/// than to one with an empty id; or nothing, if its content cannot be
/// embedded.
pub fn normalised(object: &Value) -> Option<Value> {
    let content = object["properties"]["content"].as_str()?;
    VectorizableText::try_new(content).ok()?;
    let mut object = object.clone();
    if object["properties"]["identityId"] == "" {
        object["properties"]["identityId"] = Value::Null;
    }
    Some(object)
}

/// When `object` was made, if it says.
fn created(object: &Value) -> Option<DateTime<Utc>> {
    let at = object["properties"]["createdAt"].as_str()?;
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// What to merge into an object holding `mine` for it to hold `other`:
/// `other`, and a null for each property `mine` has that `other` has not,
/// which a merge takes away. Without those a property emptied in the
/// source would stay in the target, and be found different, every time.
fn patch_of(mine: &Value, other: &Value) -> Value {
    let mut patch = other.clone();
    let gone = mine
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| {
            !value.is_null() && other.get(key.as_str()).is_none_or(Value::is_null)
        });
    for (key, _) in gone {
        patch[key.as_str()] = Value::Null;
    }
    patch
}

fn deleted() -> Value {
    serde_json::json!({ "deleted": true })
}

fn id_of(object: &Value) -> String {
    object["id"].as_str().unwrap_or_default().to_string()
}

/// `objects` with the vectors `target` stores, without any they came
/// with; or as they are, if the target embeds for itself.
async fn vectored(target: &dyn SyncTarget, objects: &[Value]) -> Result<Vec<Value>> {
    let texts: Vec<String> = objects
        .iter()
        .map(|o| {
            o["properties"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let stripped = objects.iter().map(|o| {
        let mut o = o.clone();
        o.as_object_mut().map(|o| o.remove("vector"));
        o
    });
    Ok(match target.vectors(&texts).await? {
        // A vector short, and every one after it would be another
        // memory's.
        Some(vectors) if vectors.len() != texts.len() => {
            return Err(eyre::eyre!(
                "the embedder gave {} vectors for {} memories",
                vectors.len(),
                texts.len()
            ))
        }
        Some(vectors) => stripped
            .zip(vectors)
            .map(|(mut o, v)| {
                o["vector"] = serde_json::json!(v);
                o
            })
            .collect(),
        None => stripped.collect(),
    })
}

/// Property maps equal, ignoring keys that are null on either side (an
/// export omits them), and taking RFC 3339 dates as the instants they
/// name, however a store writes them.
pub fn same_properties(a: &Value, b: &Value) -> bool {
    let set = |v: &Value| -> std::collections::BTreeMap<String, Value> {
        v.as_object()
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), instant(v)))
                    .collect()
            })
            .unwrap_or_default()
    };
    set(a) == set(b)
}

/// `value` as one form of the instant it names, if it is an RFC 3339 date.
fn instant(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map_or_else(
            || value.clone(),
            |at| Value::from(at.with_timezone(&Utc).to_rfc3339()),
        )
}

/// A store in memory that sync tests copy to and from, in this crate and
/// in those that drive a sync (`testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A store that keeps its objects in memory: embedding for itself when
    /// it has no `model`, and holding the hub's vectors when it has.
    #[derive(Debug, Default)]
    pub struct Held {
        pub model: Option<String>,
        pub objects: Mutex<Vec<Value>>,
        pub switched: Mutex<bool>,
        pub embedded: Mutex<Vec<String>>,
        /// Pages to give out before the switch marker appears, as if
        /// another replica switched while a sync was reading.
        pub switch_after: Mutex<Option<usize>>,
        /// How many of each batch it is sent the store leaves out, saying
        /// only how many it took.
        pub leaves_out: Mutex<usize>,
        /// Ids it says it cannot store, storing the rest.
        pub refuses: Mutex<Vec<String>>,
        /// How many vectors too few its embedder gives.
        pub vectors_short: Mutex<usize>,
        pub promoted: Mutex<bool>,
        /// When set, every read and write asserts a turn is held.
        pub turn: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
        /// Pages read with their vectors.
        pub vector_pages: Mutex<usize>,
        /// Ids read by [`SyncSource::objects_by_id`].
        pub fetched: Mutex<Vec<String>>,
        /// Pages to give out before the promotion marker appears, as if a
        /// hub promoted the store while a sync was reading.
        pub promote_after: Mutex<Option<usize>>,
    }

    impl Held {
        pub fn holding(model: Option<&str>, objects: Vec<Value>) -> Self {
            Self {
                model: model.map(String::from),
                objects: Mutex::new(objects),
                ..Self::default()
            }
        }

        fn in_turn(&self) {
            if let Some(turn) = self.turn.lock().unwrap().as_ref() {
                assert!(
                    turn.load(std::sync::atomic::Ordering::SeqCst),
                    "read or written outside a turn"
                );
            }
        }

        pub fn get(&self, id: &str) -> Option<Value> {
            self.objects
                .lock()
                .unwrap()
                .iter()
                .find(|o| o["id"] == id)
                .cloned()
        }
    }

    #[async_trait]
    impl SyncSource for Held {
        async fn export_page(&self, after: Option<&str>, with_vectors: bool) -> Result<Vec<Value>> {
            self.in_turn();
            if with_vectors {
                *self.vector_pages.lock().unwrap() += 1;
            }
            if let Some(pages) = self.switch_after.lock().unwrap().as_mut() {
                *pages = pages.saturating_sub(1);
                if *pages == 0 {
                    *self.switched.lock().unwrap() = true;
                }
            }
            if let Some(pages) = self.promote_after.lock().unwrap().as_mut() {
                *pages = pages.saturating_sub(1);
                if *pages == 0 {
                    *self.promoted.lock().unwrap() = true;
                }
            }
            let mut objects = self.objects.lock().unwrap().clone();
            objects.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            Ok(objects
                .into_iter()
                .filter(|o| after.is_none_or(|after| o["id"].as_str() > Some(after)))
                .take(EXPORT_PAGE)
                .map(|mut o| {
                    if !with_vectors {
                        o.as_object_mut().map(|o| o.remove("vector"));
                    }
                    o
                })
                .collect())
        }

        fn embedding_model(&self) -> Option<&str> {
            self.model.as_deref()
        }

        async fn objects_of_memory(
            &self,
            memory_id: &str,
            with_vectors: bool,
        ) -> Result<Vec<Value>> {
            self.in_turn();
            Ok(self
                .objects
                .lock()
                .unwrap()
                .iter()
                .filter(|o| o["properties"]["memoryId"] == memory_id)
                .cloned()
                .map(|mut o| {
                    if !with_vectors {
                        o.as_object_mut().map(|o| o.remove("vector"));
                    }
                    o
                })
                .collect())
        }

        async fn objects_by_id(&self, ids: &[String], with_vectors: bool) -> Result<Vec<Value>> {
            self.in_turn();
            self.fetched.lock().unwrap().extend(ids.iter().cloned());
            Ok(ids
                .iter()
                .filter_map(|id| self.get(id))
                .map(|mut o| {
                    if !with_vectors {
                        o.as_object_mut().map(|o| o.remove("vector"));
                    }
                    o
                })
                .collect())
        }
    }

    #[async_trait]
    impl SyncTarget for Held {
        async fn upsert(&self, objects: &[Value]) -> Result<usize> {
            self.in_turn();
            let refuses = self.refuses.lock().unwrap().clone();
            let (refused, objects): (Vec<&Value>, Vec<&Value>) = objects
                .iter()
                .partition(|o| refuses.iter().any(|id| o["id"] == id.as_str()));
            // What cannot be embedded is passed over, as by every store,
            // and is not counted as stored.
            let objects: Vec<&Value> = objects
                .into_iter()
                .filter(|o| normalised(o).is_some())
                .collect();
            let left_out = (*self.leaves_out.lock().unwrap()).min(objects.len());
            let objects = &objects[left_out..];
            let mut held = self.objects.lock().unwrap();
            for object in objects {
                let object = *object;
                let mut object = object.clone();
                if self.model.is_none() {
                    object.as_object_mut().map(|o| o.remove("vector"));
                }
                held.retain(|o| o["id"] != object["id"]);
                held.push(object);
            }
            let failed = refused
                .iter()
                .map(|o| (id_of(o), "it will not be stored".to_string()))
                .collect();
            imported(objects.len(), failed)
        }

        async fn patch(&self, id: &str, properties: Value) -> Result<()> {
            self.in_turn();
            let mut held = self.objects.lock().unwrap();
            if let Some(object) = held.iter_mut().find(|o| o["id"] == id) {
                for (key, value) in properties.as_object().into_iter().flatten() {
                    object["properties"][key] = value.clone();
                }
            }
            Ok(())
        }

        async fn delete(&self, id: &str) -> Result<()> {
            self.in_turn();
            self.objects.lock().unwrap().retain(|o| o["id"] != id);
            Ok(())
        }

        async fn vectors(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
            if self.model.is_none() {
                return Ok(None);
            }
            self.embedded.lock().unwrap().extend(texts.iter().cloned());
            let short = *self.vectors_short.lock().unwrap();
            let made = texts.iter().map(|t| vec![t.len() as f32, 0.0]);
            Ok(Some(made.take(texts.len().saturating_sub(short)).collect()))
        }

        async fn switched(&self) -> Result<bool> {
            Ok(*self.switched.lock().unwrap())
        }

        async fn mark_switched(&self) -> Result<()> {
            *self.switched.lock().unwrap() = true;
            Ok(())
        }

        async fn promoted(&self) -> Result<bool> {
            Ok(*self.promoted.lock().unwrap())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::testing::Held;
    use super::*;

    pub const ID1: &str = "00000000-0000-0000-0000-000000000001";
    pub const ID2: &str = "00000000-0000-0000-0000-000000000002";
    pub const ID3: &str = "00000000-0000-0000-0000-000000000003";

    pub fn obj(
        id: &str,
        content: &str,
        deleted: bool,
        accessed: u64,
        vector: Option<Vec<f32>>,
    ) -> Value {
        let mut object = serde_json::json!({
            "id": id,
            "class": "Memories",
            "properties": { "content": content, "deleted": deleted, "accessCount": accessed },
        });
        if let Some(vector) = vector {
            object["vector"] = serde_json::json!(vector);
        }
        object
    }

    fn many(ids: std::ops::Range<u64>) -> Vec<Value> {
        ids.map(|n| {
            let id = format!("00000000-0000-0000-0000-{n:012}");
            obj(&id, &format!("note {n}"), false, 0, Some(vec![1.0, 1.0]))
        })
        .collect()
    }

    /// A memory that cannot be embedded is passed over by the store it is
    /// sent to, as by every import, and is not one the store failed to
    /// take: the sync does not fail for it. A catch-up sends such a one
    /// when the target holds it without a vector.
    #[tokio::test]
    async fn a_memory_that_cannot_be_embedded_is_not_one_the_store_left_out() {
        let source = Held::holding(
            None,
            vec![
                obj(ID1, "one", false, 0, None),
                obj(ID2, "two", false, 0, None),
            ],
        );
        let target = Held::holding(
            Some("a-model"),
            vec![
                obj(ID1, "!?", false, 0, None),
                obj(ID2, "two", false, 0, None),
            ],
        );
        let report = sync(&target, &source, SyncMode::CatchUp).await.unwrap();
        assert_eq!(report.copied, 1, "{report:?}");
        assert!(
            target.get(ID2).unwrap()["vector"].is_array(),
            "embedded now"
        );
        assert_eq!(target.get(ID1).unwrap()["properties"]["content"], "!?");
        assert!(!target.get(ID1).unwrap()["vector"].is_array());

        // One the store does leave out is still a failure beside it.
        *target.leaves_out.lock().unwrap() = 1;
        target.objects.lock().unwrap().iter_mut().for_each(|o| {
            o.as_object_mut().map(|o| o.remove("vector"));
        });
        let failed = sync(&target, &source, SyncMode::CatchUp).await.unwrap_err();
        assert!(failed.to_string().contains("0 of 1"), "{failed}");
    }

    /// A sync that stored fewer memories than it sent fails, whether the
    /// store says which it could not take or only how many it took; what
    /// it could take is stored. Whoever asked for the sync does not record
    /// a switch over a failure.
    #[tokio::test]
    async fn a_sync_that_stores_fewer_than_it_sent_fails() {
        let source = Held::holding(
            None,
            vec![
                obj(ID1, "one", false, 0, None),
                obj(ID2, "two", false, 0, None),
                obj(ID3, "three", false, 0, None),
            ],
        );
        let refusing = Held::default();
        refusing.refuses.lock().unwrap().push(ID2.to_string());
        let failed = sync(&refusing, &source, SyncMode::Mirror)
            .await
            .unwrap_err();
        let short = failed
            .downcast_ref::<ImportShort>()
            .expect("which, and why");
        assert_eq!(short.stored, 2);
        assert_eq!(short.failed.len(), 1);
        assert_eq!(short.failed[0].0, ID2);
        let said = failed.to_string();
        assert!(said.contains("1 of 3") && said.contains(ID2), "{said}");
        assert!(refusing.get(ID1).is_some() && refusing.get(ID3).is_some());
        assert!(refusing.get(ID2).is_none());
        assert!(!refusing.switched().await.unwrap());

        let silent = Held::default();
        *silent.leaves_out.lock().unwrap() = 1;
        let failed = sync(&silent, &source, SyncMode::Mirror).await.unwrap_err();
        assert!(failed.to_string().contains("took 2 of 3"), "{failed}");

        // Put right, the same sync passes.
        refusing.refuses.lock().unwrap().clear();
        let done = sync(&refusing, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((done.copied, done.unchanged), (1, 2));
    }

    /// An embedder that gives fewer vectors than it was sent texts fails
    /// the sync: stored against the wrong texts they would each be another
    /// memory's.
    #[tokio::test]
    async fn an_embedder_that_gives_too_few_vectors_fails_the_sync() {
        let source = Held::holding(
            None,
            vec![
                obj(ID1, "one", false, 0, None),
                obj(ID2, "two", false, 0, None),
            ],
        );
        let target = Held::holding(Some("hub"), Vec::new());
        *target.vectors_short.lock().unwrap() = 1;
        let failed = sync(&target, &source, SyncMode::Mirror).await.unwrap_err();
        assert!(
            failed.to_string().contains("1 vectors for 2 memories"),
            "{failed}"
        );
        assert!(target.objects.lock().unwrap().is_empty(), "nothing stored");
    }

    /// A mirror that finds a memory's content changed stores it again
    /// with a vector of the new content: patched, it would keep the vector
    /// of the old, and be found by what it no longer says.
    #[tokio::test]
    async fn a_mirror_embeds_again_what_changed_its_content() {
        let source = Held::holding(None, vec![obj(ID1, "the new content", false, 0, None)]);
        let target = Held::holding(
            Some("hub"),
            vec![obj(ID1, "old", false, 0, Some(vec![3.0, 0.0]))],
        );
        let done = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((done.copied, done.updated), (1, 0));
        let held = target.get(ID1).unwrap();
        assert_eq!(held["properties"]["content"], "the new content");
        assert_eq!(held["vector"], serde_json::json!([15.0, 0.0]));
        assert_eq!(*target.embedded.lock().unwrap(), ["the new content"]);
    }

    /// A property a memory had and has no longer is taken off its copy,
    /// and the next sync finds nothing to do: left on, it would be found
    /// different, and patched in vain, every time.
    #[tokio::test]
    async fn a_property_emptied_in_the_source_is_emptied_in_the_copy() {
        let mut emptied = obj(ID1, "one", false, 0, None);
        emptied["properties"]["summary"] = Value::Null;
        let source = Held::holding(None, vec![emptied]);
        let mut with = obj(ID1, "one", false, 0, None);
        with["properties"]["summary"] = "a summary since taken away".into();
        with["properties"]["sessionKey"] = "s1".into();
        let target = Held::holding(None, vec![with]);

        let done = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((done.updated, done.unchanged), (1, 0));
        let held = target.get(ID1).unwrap();
        assert!(held["properties"]["summary"].is_null(), "{held}");
        assert!(held["properties"]["sessionKey"].is_null(), "{held}");
        let again = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((again.updated, again.unchanged), (0, 1));
    }

    /// A memory held with no vector, where the hub makes the vectors (its
    /// embedding failed when it was written), is embedded by the next
    /// sync, whatever else is so: a mirror with the source's memory, a
    /// catch-up with the target's own, which may be the newer.
    #[tokio::test]
    async fn a_memory_held_without_its_vector_is_embedded_by_the_next_sync() {
        let source = Held::holding(
            None,
            vec![
                obj(ID1, "one", false, 0, None),
                obj(ID2, "two", false, 0, None),
            ],
        );
        let held = || {
            Held::holding(
                Some("hub"),
                vec![
                    obj(ID1, "one, as written here since", false, 0, None),
                    obj(ID2, "two", false, 0, Some(vec![3.0, 0.0])),
                ],
            )
        };
        let mirrored = held();
        let done = sync(&mirrored, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((done.copied, done.unchanged), (1, 1));
        let now = mirrored.get(ID1).unwrap();
        assert_eq!(now["properties"]["content"], "one");
        assert_eq!(now["vector"], serde_json::json!([3.0, 0.0]));

        let caught_up = held();
        let done = sync(&caught_up, &source, SyncMode::CatchUp).await.unwrap();
        assert_eq!((done.copied, done.unchanged), (1, 1));
        let now = caught_up.get(ID1).unwrap();
        assert_eq!(now["properties"]["content"], "one, as written here since");
        assert_eq!(now["vector"], serde_json::json!([26.0, 0.0]));
        assert_eq!(
            *caught_up.embedded.lock().unwrap(),
            ["one, as written here since"]
        );
        // Embedded, there is nothing more to do.
        let again = sync(&caught_up, &source, SyncMode::CatchUp).await.unwrap();
        assert_eq!((again.copied, again.unchanged), (0, 2));
    }

    #[tokio::test]
    async fn a_mirror_that_finds_the_switch_made_meanwhile_drops_and_changes_nothing() {
        let source = Held::holding(Some("bge"), vec![]);
        let target = Held::holding(Some("bge"), many(0..300));
        *target.switch_after.lock().unwrap() = Some(1);
        let report = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(report.dropped, 0, "{report:?}");
        assert_eq!(target.objects.lock().unwrap().len(), 300);
    }

    #[tokio::test]
    async fn an_empty_identity_is_copied_as_none_and_not_changed_back_again() {
        let mut legacy = obj(ID1, "one", false, 0, None);
        legacy["properties"]["identityId"] = serde_json::json!("");
        let source = Held::holding(None, vec![legacy]);
        let target = Held::holding(Some("bge"), vec![]);
        sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            target.get(ID1).unwrap()["properties"]["identityId"],
            Value::Null
        );
        let again = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((again.updated, again.unchanged), (0, 1), "{again:?}");
    }

    #[tokio::test]
    async fn objects_without_ids_or_text_to_embed_are_passed_over() {
        let mut nameless = obj(ID2, "two", false, 0, None);
        nameless.as_object_mut().unwrap().remove("id");
        let source = Held::holding(
            None,
            vec![
                nameless,
                obj(ID1, "one", false, 0, None),
                obj(ID3, "  ", false, 0, None),
            ],
        );
        let target = Held::holding(Some("bge"), vec![]);
        let report = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(report.copied, 1, "{report:?}");
        assert!(target.get(ID1).is_some());
    }

    /// A store that gives its objects out of id order.
    struct Unordered;

    #[async_trait]
    impl SyncSource for Unordered {
        async fn export_page(&self, after: Option<&str>, _: bool) -> Result<Vec<Value>> {
            Ok(match after {
                None => vec![
                    obj(ID2, "two", false, 0, None),
                    obj(ID1, "one", false, 0, None),
                ],
                Some(_) => vec![],
            })
        }

        fn embedding_model(&self) -> Option<&str> {
            None
        }

        async fn objects_of_memory(&self, _: &str, _: bool) -> Result<Vec<Value>> {
            Ok(Vec::new())
        }

        async fn objects_by_id(&self, _: &[String], _: bool) -> Result<Vec<Value>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_store_out_of_id_order_is_refused() {
        let target = Held::holding(Some("bge"), vec![]);
        let refused = sync(&target, &Unordered, SyncMode::Mirror)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("not in id order"), "{refused}");
    }

    #[tokio::test]
    async fn a_memory_without_a_vector_is_embedded_where_vectors_are_copied() {
        let source = Held::holding(
            Some("bge"),
            vec![
                obj(ID1, "one", false, 0, Some(vec![7.0, 7.0])),
                obj(ID2, "two", false, 0, None),
            ],
        );
        let target = Held::holding(Some("bge"), vec![]);
        sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            target.get(ID1).unwrap()["vector"],
            serde_json::json!([7.0, 7.0])
        );
        assert_eq!(
            target.get(ID2).unwrap()["vector"],
            serde_json::json!([3.0, 0.0])
        );
        assert_eq!(*target.embedded.lock().unwrap(), ["two"]);
    }

    #[tokio::test]
    async fn carrying_back_drops_what_was_made_since_the_switch_and_erased_since() {
        let since: DateTime<Utc> = "2026-10-07T12:00:00Z".parse().unwrap();
        let made = |id, at: &str| {
            let mut object = obj(id, "note", false, 0, None);
            object["properties"]["createdAt"] = serde_json::json!(at);
            object
        };
        let current = Held::holding(Some("bge"), vec![]);
        let previous = Held::holding(
            None,
            vec![
                made(ID1, "2026-10-06T09:00:00Z"),
                made(ID2, "2026-10-07T12:30:00Z"),
            ],
        );
        let report = sync(&previous, &current, SyncMode::Back { since })
            .await
            .unwrap();
        assert_eq!(report.dropped, 1, "{report:?}");
        assert!(previous.get(ID1).is_some(), "made before the switch: kept");
        assert!(previous.get(ID2).is_none(), "made since, and erased since");
    }

    #[test]
    fn a_date_written_another_way_does_not_count_as_a_change() {
        assert!(same_properties(
            &serde_json::json!({ "createdAt": "2026-07-05T09:30:00.1Z", "n": 1 }),
            &serde_json::json!({ "createdAt": "2026-07-05T10:30:00.100+01:00", "n": 1 })
        ));
        assert!(!same_properties(
            &serde_json::json!({ "createdAt": "2026-07-05T09:30:00.1Z" }),
            &serde_json::json!({ "createdAt": "2026-07-05T09:30:00.2Z" })
        ));
    }

    #[test]
    fn nulls_do_not_count_as_a_change() {
        assert!(same_properties(
            &serde_json::json!({ "a": 1, "b": null }),
            &serde_json::json!({ "a": 1 })
        ));
        assert!(!same_properties(
            &serde_json::json!({ "a": 1 }),
            &serde_json::json!({ "a": 2 })
        ));
    }

    #[tokio::test]
    async fn a_mirror_over_many_pages_matches_both_sides_by_id() {
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let model = Some("bge");
        let vector = || Some(vec![1.0, 1.0]);
        // Of 1000 ids, the source holds the even ones and the target every
        // third; the target holds those it shares with the source changed
        // every fifth time.
        let source = Held::holding(
            model,
            (0..1000_u64)
                .filter(|n| n % 2 == 0)
                .map(|n| obj(&id(n), &format!("note {n}"), false, n, vector()))
                .collect(),
        );
        let target = Held::holding(
            model,
            (0..1000_u64)
                .filter(|n| n % 3 == 0)
                .map(|n| {
                    let accessed = if n % 5 == 0 { n + 1 } else { n };
                    obj(&id(n), &format!("note {n}"), false, accessed, vector())
                })
                .collect(),
        );
        let shared = (0..1000_u64).filter(|n| n % 6 == 0);
        let report = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            report,
            SyncReport {
                copied: 500 - shared.clone().count(),
                updated: shared.clone().filter(|n| n % 5 == 0).count(),
                dropped: (0..1000_u64).filter(|n| n % 3 == 0 && n % 2 != 0).count(),
                unchanged: shared.filter(|n| n % 5 != 0).count(),
            }
        );
        let mut held: Vec<Value> = target.objects.lock().unwrap().clone();
        held.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let mut wanted: Vec<Value> = source.objects.lock().unwrap().clone();
        wanted.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        assert_eq!(held, wanted, "the target ends as the source");
    }

    #[tokio::test]
    async fn a_mirror_copies_updates_and_drops_keeping_ids_and_embedding_what_changed() {
        let source = Held::holding(
            None,
            vec![
                obj(ID1, "one", false, 5, None),
                obj(ID2, "two", false, 0, None),
            ],
        );
        let target = Held::holding(
            Some("bge"),
            vec![
                obj(ID1, "one", false, 1, Some(vec![9.0, 9.0])),
                obj(ID3, "gone", false, 0, Some(vec![9.0, 9.0])),
            ],
        );
        let report = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            report,
            SyncReport {
                copied: 1,
                updated: 1,
                dropped: 1,
                unchanged: 0
            }
        );
        assert_eq!(
            target.get(ID2).unwrap()["vector"],
            serde_json::json!([3.0, 0.0])
        );
        let one = target.get(ID1).unwrap();
        assert_eq!(one["properties"]["accessCount"], 5);
        assert_eq!(
            one["vector"],
            serde_json::json!([9.0, 9.0]),
            "patched, not embedded again"
        );
        assert!(target.get(ID3).is_none());
        assert_eq!(*target.embedded.lock().unwrap(), ["two"]);
    }

    #[tokio::test]
    async fn vectors_are_copied_between_stores_of_the_same_model_and_not_otherwise() {
        let source = Held::holding(
            Some("bge"),
            vec![obj(ID1, "one", false, 0, Some(vec![1.0, 2.0]))],
        );
        let alike = Held::holding(Some("bge"), vec![]);
        sync(&alike, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            alike.get(ID1).unwrap()["vector"],
            serde_json::json!([1.0, 2.0])
        );
        assert!(alike.embedded.lock().unwrap().is_empty());

        let other = Held::holding(Some("titan"), vec![]);
        sync(&other, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!(
            other.get(ID1).unwrap()["vector"],
            serde_json::json!([3.0, 0.0])
        );

        let self_embedding = Held::holding(None, vec![]);
        sync(&self_embedding, &source, SyncMode::Mirror)
            .await
            .unwrap();
        assert!(self_embedding.get(ID1).unwrap().get("vector").is_none());
    }

    #[tokio::test]
    async fn a_mirror_after_another_replica_switched_keeps_what_it_wrote() {
        let source = Held::holding(None, vec![obj(ID1, "one", false, 0, None)]);
        let target = Held::holding(
            Some("bge"),
            vec![
                obj(ID1, "one", false, 7, Some(vec![1.0, 0.0])),
                obj(
                    ID2,
                    "written after the switch",
                    false,
                    0,
                    Some(vec![1.0, 0.0]),
                ),
            ],
        );
        target.mark_switched().await.unwrap();
        let report = sync(&target, &source, SyncMode::Mirror).await.unwrap();
        assert_eq!((report.updated, report.dropped), (0, 0));
        assert!(target.get(ID2).is_some());
        assert_eq!(target.get(ID1).unwrap()["properties"]["accessCount"], 7);
    }

    #[tokio::test]
    async fn a_catch_up_adds_new_memories_and_forgetting_only() {
        let source = Held::holding(
            None,
            vec![
                obj(ID1, "one", true, 0, None),
                obj(ID2, "two", false, 0, None),
                obj(ID3, "three", false, 9, None),
            ],
        );
        let target = Held::holding(
            Some("bge"),
            vec![
                obj(ID1, "one", false, 0, Some(vec![1.0, 0.0])),
                obj(ID3, "three", true, 2, Some(vec![1.0, 0.0])),
            ],
        );
        let report = sync(&target, &source, SyncMode::CatchUp).await.unwrap();
        assert_eq!(
            report,
            SyncReport {
                copied: 1,
                updated: 1,
                dropped: 0,
                unchanged: 1
            }
        );
        assert_eq!(target.get(ID1).unwrap()["properties"]["deleted"], true);
        assert_eq!(
            target.get(ID3).unwrap()["properties"]["accessCount"],
            2,
            "never undone"
        );
        assert!(target.get(ID2).is_some());
    }

    #[tokio::test]
    async fn memories_written_after_a_switch_are_carried_back_to_the_previous_store() {
        let previous = Held::holding(None, vec![obj(ID1, "one", false, 0, None)]);
        let next = Held::holding(Some("bge"), vec![]);
        sync(&next, &previous, SyncMode::Mirror).await.unwrap();
        next.mark_switched().await.unwrap();
        next.upsert(&[obj(
            ID2,
            "new since the switch",
            false,
            0,
            Some(vec![1.0, 0.0]),
        )])
        .await
        .unwrap();
        next.patch(ID1, serde_json::json!({ "deleted": true }))
            .await
            .unwrap();

        let back = sync(&previous, &next, SyncMode::CatchUp).await.unwrap();
        assert_eq!((back.copied, back.updated), (1, 1));
        let carried = previous.get(ID2).unwrap();
        assert_eq!(carried["properties"]["content"], "new since the switch");
        assert!(
            carried.get("vector").is_none(),
            "the previous store embeds for itself"
        );
        assert_eq!(previous.get(ID1).unwrap()["properties"]["deleted"], true);

        let again = sync(&next, &previous, SyncMode::CatchUp).await.unwrap();
        assert_eq!(again.copied, 0, "nothing comes back a second time");
    }

    /// A memory `memory_id` held by the object `id`.
    fn mem(
        id: &str,
        memory_id: &str,
        content: &str,
        accessed: u64,
        vector: Option<Vec<f32>>,
    ) -> Value {
        let mut object = obj(id, content, false, accessed, vector);
        object["properties"]["memoryId"] = memory_id.into();
        object
    }

    fn sorted(store: &Held) -> Vec<Value> {
        let mut held = store.objects.lock().unwrap().clone();
        held.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        held
    }

    /// The switch marker comes along when vellev is imported from Weaviate,
    /// and a mirror finding it runs as a catch-up, which drops nothing. A
    /// replica copies, updates and drops whatever the marker says.
    #[tokio::test]
    async fn a_replica_drops_what_the_source_no_longer_holds_though_the_switch_marker_was_copied() {
        let source = Held::holding(None, vec![obj(ID1, "one", false, 5, None)]);
        let target = Held::holding(
            None,
            vec![
                obj(ID1, "one", false, 1, None),
                obj(ID3, "purged in the source", false, 0, None),
            ],
        );
        target.mark_switched().await.unwrap();
        let report = sync(&target, &source, SyncMode::Replica).await.unwrap();
        assert_eq!(
            report,
            SyncReport {
                copied: 0,
                updated: 1,
                dropped: 1,
                unchanged: 0
            }
        );
        assert!(target.get(ID3).is_none());
        assert_eq!(target.get(ID1).unwrap()["properties"]["accessCount"], 5);
    }

    /// A memory the replica holds without its vector is stored again as
    /// the source holds it, never as the replica held it.
    #[tokio::test]
    async fn a_replica_takes_the_sources_memory_for_one_held_without_its_vector() {
        let source = Held::holding(None, vec![obj(ID1, "as the source has it", false, 0, None)]);
        let target = Held::holding(Some("hub"), vec![obj(ID1, "stale", false, 0, None)]);
        target.mark_switched().await.unwrap();
        let report = sync(&target, &source, SyncMode::Replica).await.unwrap();
        assert_eq!(report.copied, 1, "{report:?}");
        let held = target.get(ID1).unwrap();
        assert_eq!(held["properties"]["content"], "as the source has it");
        assert!(held["vector"].is_array());
    }

    /// A replica of a promoted store writes nothing to it.
    #[tokio::test]
    async fn a_replica_refuses_a_promoted_target() {
        let source = Held::holding(None, vec![obj(ID1, "one", false, 0, None)]);
        let target = Held::holding(
            None,
            vec![obj(ID3, "written since the promotion", false, 0, None)],
        );
        *target.promoted.lock().unwrap() = true;
        let refused = sync(&target, &source, SyncMode::Replica).await.unwrap_err();
        assert!(refused.downcast_ref::<Promoted>().is_some(), "{refused}");
        assert!(target.get(ID3).is_some());
        assert!(target.get(ID1).is_none());
    }

    /// A store promoted while a replica walks it (another hub promoted it
    /// during a rolling deploy) has nothing dropped from it after.
    #[tokio::test]
    async fn a_replica_stops_before_it_writes_once_the_target_is_promoted_mid_walk() {
        let source = Held::holding(None, vec![]);
        let target = Held::holding(None, many(0..300));
        *target.promote_after.lock().unwrap() = Some(1);
        let refused = sync(&target, &source, SyncMode::Replica).await.unwrap_err();
        assert!(refused.downcast_ref::<Promoted>().is_some(), "{refused}");
        assert_eq!(target.objects.lock().unwrap().len(), 300);
    }

    /// A memory the source no longer holds is taken from the replica
    /// whole: every object holding it, and nothing else.
    #[tokio::test]
    async fn syncing_a_purged_memory_removes_every_copy() {
        let source = Held::holding(None, vec![mem(ID3, "m2", "two", 0, None)]);
        let target = Held::holding(
            None,
            vec![
                mem(ID1, "m1", "one", 0, None),
                mem(ID2, "m1", "one, again", 0, None),
                mem(ID3, "m2", "two", 0, None),
            ],
        );
        let report = sync_memory(&target, &source, "m1", &NoTurns).await.unwrap();
        assert_eq!(report.dropped, 2, "{report:?}");
        assert!(target.get(ID1).is_none() && target.get(ID2).is_none());
        assert!(target.get(ID3).is_some());
    }

    /// The replica ends holding a memory under the source's object ids
    /// only: one it holds under another id is dropped, though the source
    /// still holds the memory.
    #[tokio::test]
    async fn syncing_a_memory_keeps_the_sources_object_ids_and_drops_a_stale_one() {
        let source = Held::holding(None, vec![mem(ID1, "m1", "one", 5, None)]);
        let target = Held::holding(
            None,
            vec![
                mem(ID1, "m1", "one", 1, None),
                mem(ID2, "m1", "one", 1, None),
            ],
        );
        let report = sync_memory(&target, &source, "m1", &NoTurns).await.unwrap();
        assert_eq!((report.updated, report.dropped), (1, 1), "{report:?}");
        assert_eq!(sorted(&target), sorted(&source));
    }

    #[tokio::test]
    async fn syncing_a_memory_refuses_a_promoted_target() {
        let source = Held::holding(None, vec![]);
        let target = Held::holding(None, vec![mem(ID1, "m1", "one", 0, None)]);
        *target.promoted.lock().unwrap() = true;
        let refused = sync_memory(&target, &source, "m1", &NoTurns)
            .await
            .unwrap_err();
        assert!(refused.downcast_ref::<Promoted>().is_some(), "{refused}");
        assert!(target.get(ID1).is_some());
    }

    /// A rollback from the promoted store carries every change back:
    /// copies, updates, content rewritten, a memory erased, duplicates
    /// dropped. The target's switch marker (Weaviate's is there) does not
    /// make it a catch-up, nor does it say it is promoted; and a second
    /// pass has nothing left to do.
    #[tokio::test]
    async fn a_rollback_carries_back_every_change_made_since_the_promotion() {
        let (source, target) = replica_case();
        assert!(*target.switched.lock().unwrap());
        // As a store that must not be asked: a replica would stop here.
        *target.promoted.lock().unwrap() = true;
        sync(&target, &source, SyncMode::Rollback).await.unwrap();
        assert_eq!(sorted(&target), sorted(&source));
        let again = sync(&target, &source, SyncMode::Rollback).await.unwrap();
        assert_eq!(
            (again.copied, again.updated, again.dropped),
            (0, 0, 0),
            "{again:?}"
        );
    }

    /// The memories, and the replica's copies of them, the next test syncs
    /// one at a time and whole: one copied, one changed, one with its
    /// content changed, one held without a vector, one purged, one with a
    /// stale duplicate, one in line.
    fn replica_case() -> (Held, Held) {
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let v = || Some(vec![1.0, 1.0]);
        let source = Held::holding(
            Some("hub"),
            vec![
                mem(&id(1), "new", "new", 0, v()),
                mem(&id(2), "changed", "changed", 7, v()),
                mem(&id(3), "rewritten", "rewritten now", 0, v()),
                mem(&id(4), "unvectored", "unvectored", 0, v()),
                mem(&id(6), "duplicated", "duplicated", 0, v()),
                mem(&id(8), "in-line", "in line", 0, v()),
                mem(&id(9), "two-objects", "two objects", 0, v()),
                mem(&id(10), "two-objects", "two objects", 1, v()),
            ],
        );
        let target = Held::holding(
            Some("hub"),
            vec![
                mem(&id(2), "changed", "changed", 1, v()),
                mem(&id(3), "rewritten", "rewritten", 0, v()),
                mem(&id(4), "unvectored", "unvectored", 0, None),
                mem(&id(5), "purged", "purged", 0, v()),
                mem(&id(6), "duplicated", "duplicated", 0, v()),
                mem(&id(7), "duplicated", "duplicated", 0, v()),
                mem(&id(8), "in-line", "in line", 0, v()),
                mem(&id(9), "two-objects", "two objects", 0, v()),
            ],
        );
        *target.switched.lock().unwrap() = true;
        (source, target)
    }

    const REPLICA_MEMORIES: [&str; 7] = [
        "new",
        "changed",
        "rewritten",
        "unvectored",
        "purged",
        "duplicated",
        "in-line",
    ];

    /// The objects of `store` holding `memory_id`.
    fn of(store: &Held, memory_id: &str) -> Vec<Value> {
        sorted(store)
            .into_iter()
            .filter(|o| o["properties"]["memoryId"] == memory_id)
            .collect()
    }

    /// A memory synced alone ends as a replica of the whole store leaves
    /// it, and nothing else is touched; after a whole replica, a memory
    /// synced alone has nothing left to do. The per-memory sync and the
    /// reconcile never undo each other's work.
    #[tokio::test]
    async fn a_memory_synced_alone_ends_as_a_whole_replica_leaves_it() {
        let (source, whole) = replica_case();
        sync(&whole, &source, SyncMode::Replica).await.unwrap();
        assert_eq!(
            sorted(&whole),
            sorted(&source),
            "the whole replica is the source"
        );
        for memory in REPLICA_MEMORIES.into_iter().chain(["two-objects"]) {
            let (_, alone) = replica_case();
            let before = sorted(&alone);
            sync_memory(&alone, &source, memory, &NoTurns)
                .await
                .unwrap();
            assert_eq!(of(&alone, memory), of(&whole, memory), "{memory}");
            let others = |held: Vec<Value>| -> Vec<Value> {
                held.into_iter()
                    .filter(|o| o["properties"]["memoryId"] != memory)
                    .collect()
            };
            assert_eq!(
                others(sorted(&alone)),
                others(before),
                "{memory}: others untouched"
            );

            let again = sync_memory(&whole, &source, memory, &NoTurns)
                .await
                .unwrap();
            assert_eq!(
                (again.copied, again.updated, again.dropped),
                (0, 0, 0),
                "{memory}: {again:?}"
            );
        }
        let again = sync(&whole, &source, SyncMode::Replica).await.unwrap();
        assert_eq!((again.copied, again.updated, again.dropped), (0, 0, 0));
    }

    /// Turns that mark when one is held, and run `on_take` with each
    /// turn's number (from 1) before it is given.
    #[derive(Default)]
    struct Turns {
        held: Arc<std::sync::atomic::AtomicBool>,
        taken: Mutex<usize>,
        on_take: Mutex<Option<Box<dyn FnMut(usize) + Send>>>,
    }

    struct Taken(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl WriteTurns for Turns {
        async fn take(&self) -> Result<Box<dyn Turn>> {
            let n = {
                let mut taken = self.taken.lock().unwrap();
                *taken += 1;
                *taken
            };
            if let Some(on_take) = self.on_take.lock().unwrap().as_mut() {
                on_take(n);
            }
            assert!(
                !self.held.swap(true, std::sync::atomic::Ordering::SeqCst),
                "one turn at a time"
            );
            Ok(Box::new(Taken(Arc::clone(&self.held))))
        }
    }

    #[async_trait]
    impl Turn for Taken {
        async fn done(self: Box<Self>) -> Result<()> {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Every read and write a replica makes is made within a turn, and no
    /// turn spans the whole walk: an erasure waits for one page at most.
    #[tokio::test]
    async fn a_replica_reads_and_writes_only_within_turns_of_a_page() {
        let source = Held::holding(None, many(0..250));
        let target = Held::holding(None, many(1000..1100));
        let turns = Turns::default();
        *source.turn.lock().unwrap() = Some(Arc::clone(&turns.held));
        *target.turn.lock().unwrap() = Some(Arc::clone(&turns.held));
        let report = sync_with(&target, &source, SyncMode::Replica, &turns)
            .await
            .unwrap();
        assert_eq!((report.copied, report.dropped), (250, 100), "{report:?}");
        assert!(
            *turns.taken.lock().unwrap() >= 4,
            "turns: {}",
            turns.taken.lock().unwrap()
        );

        let one = Turns::default();
        *source.turn.lock().unwrap() = Some(Arc::clone(&one.held));
        *target.turn.lock().unwrap() = Some(Arc::clone(&one.held));
        target.objects.lock().unwrap().push(mem(
            &format!("00000000-0000-0000-0000-{:012}", 2000),
            "gone",
            "gone",
            0,
            None,
        ));
        let report = sync_memory(&target, &source, "gone", &one).await.unwrap();
        assert_eq!(report.dropped, 1);
        assert_eq!(*one.taken.lock().unwrap(), 1);
    }

    /// A memory erased between two turns is not copied from what a turn
    /// before the erasure read: each turn reads afresh what it decides on.
    #[tokio::test]
    async fn a_memory_erased_between_turns_is_not_copied_from_a_read_before_it() {
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let source = Arc::new(Held::holding(
            None,
            (0..400_u64)
                .filter(|n| n % 2 == 0)
                .map(|n| obj(&id(n), &format!("note {n}"), false, 0, None))
                .collect(),
        ));
        let target = Held::holding(
            None,
            (0..100_u64)
                .filter(|n| n % 2 == 1)
                .map(|n| obj(&id(n), &format!("note {n}"), false, 0, None))
                .collect(),
        );
        let turns = Turns::default();
        let erasing = Arc::clone(&source);
        let erased = id(150);
        *turns.on_take.lock().unwrap() = Some(Box::new(move |n| {
            if n == 2 {
                erasing
                    .objects
                    .lock()
                    .unwrap()
                    .retain(|o| o["id"] != erased.as_str());
            }
        }));
        sync_with(&target, source.as_ref(), SyncMode::Replica, &turns)
            .await
            .unwrap();
        assert!(target.get(&id(150)).is_none(), "erased, and copied back");
        assert_eq!(sorted(&target), sorted(&source));
    }

    /// A replica walks the source without vectors, and reads the vectors,
    /// with the whole object, of only those it copies: the source serves
    /// users, and a walk of every vector every interval is load on it for
    /// nothing.
    #[tokio::test]
    async fn a_replica_reads_vectors_only_for_what_it_copies() {
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let v = |n: u64| Some(vec![n as f32, 1.0]);
        let source = Held::holding(
            Some("hub"),
            (0..150_u64)
                .map(|n| obj(&id(n), &format!("note {n}"), false, 0, v(n)))
                .collect(),
        );
        let target = Held::holding(
            Some("hub"),
            (0..150_u64)
                .filter(|n| n % 10 != 0)
                .map(|n| obj(&id(n), &format!("note {n}"), false, 0, v(n)))
                .collect(),
        );
        let report = sync(&target, &source, SyncMode::Replica).await.unwrap();
        assert_eq!((report.copied, report.unchanged), (15, 135), "{report:?}");
        assert_eq!(
            *source.vector_pages.lock().unwrap(),
            0,
            "the walk read no vectors"
        );
        let mut fetched = source.fetched.lock().unwrap().clone();
        fetched.sort();
        assert_eq!(
            fetched,
            (0..150_u64)
                .filter(|n| n % 10 == 0)
                .map(id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            sorted(&target),
            sorted(&source),
            "copied with their vectors"
        );
        assert!(
            target.embedded.lock().unwrap().is_empty(),
            "none embedded again"
        );
    }

    /// A sync that fails part way leaves in its record what it did before,
    /// each object by name and the memory it holds: a reconcile that fails
    /// still says what it fixed.
    #[tokio::test]
    async fn a_sync_that_fails_part_way_keeps_what_it_fixed() {
        let id = |n: u64| format!("00000000-0000-0000-0000-{n:012}");
        let source = Held::holding(
            None,
            (0..150_u64)
                .map(|n| mem(&id(n), &format!("m{n}"), "note", 0, None))
                .collect(),
        );
        let target = Held::holding(None, vec![]);
        target.refuses.lock().unwrap().push(id(120));
        let mut synced = Synced::default();
        let failed = sync_recorded(&target, &source, SyncMode::Replica, &NoTurns, &mut synced)
            .await
            .unwrap_err();
        assert!(failed.downcast_ref::<ImportShort>().is_some(), "{failed}");
        assert_eq!(synced.report.copied, 149, "{:?}", synced.report);
        assert_eq!(synced.fixed.len(), 149);
        assert!(synced.fixed.iter().all(|fix| fix.kind == FixKind::Copied));
        assert!(
            !synced.fixed.iter().any(|fix| fix.object_id == id(120)),
            "not stored, not fixed"
        );
        assert_eq!(
            synced.fixed[0],
            Fix {
                kind: FixKind::Copied,
                object_id: id(0),
                memory_id: Some("m0".into()),
            }
        );
    }

    /// Each kind of fix is recorded with the memory it touched: an update
    /// and a drop as well as a copy.
    #[tokio::test]
    async fn every_fix_is_recorded_with_its_memory() {
        let source = Held::holding(
            None,
            vec![
                mem(ID1, "m1", "one", 5, None),
                mem(ID2, "m2", "two", 0, None),
            ],
        );
        let target = Held::holding(
            None,
            vec![
                mem(ID1, "m1", "one", 1, None),
                mem(ID3, "m3", "three", 0, None),
            ],
        );
        let mut synced = Synced::default();
        sync_recorded(&target, &source, SyncMode::Replica, &NoTurns, &mut synced)
            .await
            .unwrap();
        let mut fixed: Vec<(FixKind, String, Option<String>)> = synced
            .fixed
            .into_iter()
            .map(|f| (f.kind, f.object_id, f.memory_id))
            .collect();
        fixed.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            fixed,
            [
                (FixKind::Updated, ID1.to_string(), Some("m1".into())),
                (FixKind::Copied, ID2.to_string(), Some("m2".into())),
                (FixKind::Dropped, ID3.to_string(), Some("m3".into())),
            ]
        );
    }
}
