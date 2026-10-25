//! DB-backed artifact knowledge base (Artifacts v2 — DW-546 / DW-549).
//!
//! Supersedes the per-agent file storage in
//! [`crate::workspace::SharedWorkspace`]. Artifacts gain an owner, ACL
//! visibility, tags, and a `snippet` type, and become semantically
//! searchable via an optional [`ArtifactIndexer`] (Weaviate, DW-551).
//!
//! Two backends implement [`ArtifactBackend`]:
//! - [`KnowledgeStore`] — Postgres, the production path: shared across
//!   agents with per-user/global ACL and (when an indexer is wired)
//!   semantic search.
//! - [`FsArtifactBackend`] — a per-agent file fallback for deployments
//!   without Postgres. Every user of the agent shares its directory, so it
//!   enforces the same ACL; search degrades to a substring match.
//!
//! Owner semantics (decided for DW-546): an artifact's `owner_id` is the
//! caller's `identity_id` when a human is in the loop, else the agent id,
//! else the `"legacy"` sentinel used by the disk→DB migration (DW-548).
//! ACL compares the same key — see [`Caller`].

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use tokio_postgres::types::ToSql;

use workforce_common::postgres::PostgresPool;

pub use crate::workspace::{ArtifactStatus, ArtifactType};

/// Read/visibility scope for an artifact.
///
/// `Team` is currently org-wide (knowledge is shared across all paired
/// users until RBAC lands) and, like `Public`, is readable by everyone;
/// the distinction is retained for forward compatibility and UI badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Team,
    Public,
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private => write!(f, "private"),
            Self::Team => write!(f, "team"),
            Self::Public => write!(f, "public"),
        }
    }
}

impl std::str::FromStr for Visibility {
    type Err = std::convert::Infallible;

    /// Unknown values fall back to the most restrictive scope (`Private`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "team" => Self::Team,
            "public" => Self::Public,
            _ => Self::Private,
        })
    }
}

/// An artifact in the v2 knowledge base.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub artifact_type: ArtifactType,
    pub title: String,
    pub content: String,
    pub owner_id: String,
    pub visibility: Visibility,
    pub tags: Vec<String>,
    pub status: ArtifactStatus,
    pub version: u32,
    pub created_by: String,
    pub last_modified_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Artifact {
    /// Build a fresh draft artifact (version 1, timestamps = now).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        artifact_type: ArtifactType,
        title: impl Into<String>,
        content: impl Into<String>,
        owner_id: impl Into<String>,
        created_by: impl Into<String>,
        visibility: Visibility,
        tags: Vec<String>,
    ) -> Self {
        let now = Utc::now();
        let created_by = created_by.into();
        Self {
            id: id.into(),
            artifact_type,
            title: title.into(),
            content: content.into(),
            owner_id: owner_id.into(),
            visibility,
            tags: normalize_tags(tags),
            status: ArtifactStatus::Draft,
            version: 1,
            last_modified_by: created_by.clone(),
            created_by,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Lowercase, trim, drop empties, and de-duplicate a tag list (stable order).
pub fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// The principal making a request, for ACL checks.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Owner key: the user's `identity_id` when present, else the agent id.
    pub key: String,
    /// Admin / owner-token callers bypass ACL (e.g. the web UI owner token).
    pub is_admin: bool,
}

impl Caller {
    /// A normal user/agent principal identified by `key`.
    pub fn user(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            is_admin: false,
        }
    }

    /// An ACL-bypassing administrator (owner token / system).
    pub fn admin() -> Self {
        Self {
            key: String::new(),
            is_admin: true,
        }
    }
}

/// Filter for [`ArtifactBackend::list`] / [`ArtifactBackend::search`].
#[derive(Debug, Clone, Default)]
pub struct ArtifactFilter {
    pub artifact_type: Option<ArtifactType>,
    pub visibility: Option<Visibility>,
    pub tag: Option<String>,
}

/// A search result with a relevance score (0.0 for non-semantic backends).
#[derive(Debug, Clone)]
pub struct ScoredArtifact {
    pub artifact: Artifact,
    pub score: f32,
}

/// Optional semantic-search / indexing hook (a vector database +
/// auto-tagger, DW-551).
///
/// Implemented out-of-crate (in the hub) so the orchestrator stays free of
/// the vector database and LLM dependencies. The index is best-effort for
/// whoever writes an artifact: the store indexes from a detached task, and
/// an index that does not take a write never fails the artifact's own. It
/// is not forgotten either: Postgres notes every write as pending in the
/// transaction that makes it, and the store puts the index right later
/// (see [`KnowledgeStore::keep_index_complete`]).
#[async_trait]
pub trait ArtifactIndexer: Send + Sync {
    /// The tags an artifact is to carry: its own and any an auto-tagger
    /// adds, normalised. Best-effort: with no tagger, or one that fails,
    /// its own.
    async fn tags(&self, artifact: &Artifact) -> Vec<String> {
        artifact.tags.clone()
    }
    /// Remove an artifact from the vector index; one it does not hold is
    /// removed already.
    async fn remove(&self, id: String) -> Result<()>;
    /// Semantic search → ranked `(id, score)` pairs. The store re-loads and
    /// ACL-filters the returned ids, so the indexer need not model visibility.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<(String, f32)>>;
    /// Embed an artifact as it is stored, its tags as they are, replacing
    /// what the index held of it, and keep `text_hash` with it: the hash of
    /// what was embedded, as the index gives it back when listed.
    async fn reindex(&self, artifact: &Artifact, text_hash: &str) -> Result<()>;
    /// Whether the index holds every artifact, so that search may rely on
    /// it alone: not while a backfill into a new index has yet to finish,
    /// and no longer once the index was lost and made again.
    async fn complete(&self) -> Result<bool>;
    /// Record that the index may not hold every artifact: what a repair
    /// does before it indexes anything, so that one that stops part of the
    /// way leaves an index nothing relies on.
    async fn mark_incomplete(&self) -> Result<()>;
    /// Record that the index holds every artifact. `Err`, recording
    /// nothing, if the index is not the one [`Self::mark_incomplete`] was
    /// last asked of: lost and made again meanwhile, it holds only part.
    async fn mark_complete(&self) -> Result<()>;
    /// Of `page`, artifacts as `(id, text hash)`, the ids of those the
    /// index lacks or holds another text of. An index that keeps no hash
    /// (see [`Self::compares_text`]) tells only those it lacks. It reads
    /// as little as it can: a page it holds exactly costs it no text.
    async fn differing(&self, page: &[(String, String)]) -> Result<Vec<String>>;
    /// Whether the index keeps each artifact's text hash, so that
    /// [`Self::differing`] tells a changed text and comparing it with
    /// Postgres from time to time is worth while.
    fn compares_text(&self) -> bool {
        true
    }
    /// Which kind of index this is, in a word: what the hubs serving it
    /// and a seeding of it name their lock by ([`KnowledgeStore::seed_index`]).
    fn backend(&self) -> &'static str {
        "index"
    }
    /// How many artifacts the index holds, asked as cheaply as it can be.
    async fn count(&self) -> Result<usize>;
    /// [`Self::search`] and [`Self::count`] together, in one request where
    /// the index can answer both at once.
    async fn search_counted(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<(String, f32)>, usize)> {
        let found = self.search(query, limit).await?;
        Ok((found, self.count().await?))
    }
    /// Why the index is not to be searched at all, if a write has found it
    /// is not the one the hub made (a class Weaviate made for itself): it
    /// is then searched by keyword until a write finds it the hub's again.
    /// Decides nothing else.
    fn refused_index(&self) -> Option<String> {
        None
    }
    /// Up to `limit` of the ids of the artifacts the index holds, from the
    /// place `after` (from the start when `None`), and the place to go on
    /// from, or `None` at the end. Read only when the index holds more
    /// than is stored, to find what to remove.
    async fn ids_after(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>)>;
}

/// Artifacts per page of a pass over them.
const BACKFILL_PAGE: i64 = 100;

/// How many times an artifact that changes while it is being indexed is
/// indexed again before the try is given up as failed.
const SETTLE_TRIES: usize = 5;

/// Failures a pass keeps the words of, for [`IndexState`].
const FAILURES_KEPT: usize = 20;

/// Pending artifacts read at a time.
const PENDING_PAGE: i64 = 500;

/// How many artifacts in a row the index may fail to take before a round
/// ends. It bounds the keeper's time at an index that takes nothing, each
/// try at which may cost a whole deadline; it says nothing about any
/// artifact, and what was tried is noted as any failure is.
pub const ROUND_ENDS_AFTER: usize = 3;

/// How long a search waits for the index before it goes by keyword.
pub const SEARCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// How long the index is left alone, searches going by keyword at once,
/// after one found it not answering.
pub const SEARCH_REST: std::time::Duration = std::time::Duration::from_secs(30);

/// The Postgres advisory lock one repair at a time holds, across every
/// process on the database: "artifact index backfill", as a key.
const BACKFILL_LOCK: i64 = 0x6172_7469_6278_6266;

/// The hash of what is indexed of an artifact (its title, tags and
/// content), as Postgres computes it: the same expression as the trigger
/// that notes a write as pending (telemetry V023), so that the two agree.
const TEXT_HASH: &str = r"md5(title || E'\x01' || tags || E'\x01' || content)";

/// The text hash noted for an artifact that was deleted.
const DELETED: &str = "";

/// How [`KnowledgeStore::keep_index_complete`] paces itself.
#[derive(Debug, Clone, Copy)]
pub struct IndexPace {
    /// The fixed wait after a round in which the index took none of what it
    /// was given. A write does not cut it short.
    pub retry: std::time::Duration,
    /// The wait after a round that left nothing undone, unless something
    /// is found to do sooner. Each round puts right what is pending and
    /// compares how many artifacts the index and Postgres hold.
    pub watch: std::time::Duration,
    /// How often the index is compared with Postgres artifact by artifact,
    /// id and text hash: what finds an index that fell behind with nothing
    /// noted, as one restored from a backup does.
    pub reconcile: std::time::Duration,
    /// How long an artifact the index did not take waits before it is
    /// tried again, the first time; doubled each time after, up to
    /// `backoff_cap`.
    pub backoff: std::time::Duration,
    /// The longest an artifact waits between tries, give or take a fifth;
    /// and how long a compare that could not put the index right holds
    /// off another for a count out of step or a lost marker.
    pub backoff_cap: std::time::Duration,
    /// How long one try at one artifact may take, whatever the index's
    /// own client allows.
    pub deadline: std::time::Duration,
}

impl IndexPace {
    /// What a hub keeps to unless told otherwise.
    pub const DEFAULT: Self = Self {
        retry: std::time::Duration::from_secs(30),
        watch: std::time::Duration::from_secs(600),
        reconcile: std::time::Duration::from_secs(3600),
        backoff: std::time::Duration::from_secs(30),
        backoff_cap: std::time::Duration::from_secs(900),
        deadline: std::time::Duration::from_secs(150),
    };
}

impl Default for IndexPace {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The two names an index's keeping goes by in Postgres: the table its
/// pending artifacts are noted in, and the lock one compare of it runs
/// under. Arguments, and not constants, so that a second index kept
/// beside the serving one (shadow mode, DW-676) has its own of each, in a
/// table made `LIKE` this one.
#[derive(Debug, Clone, Copy)]
pub struct IndexSlot {
    pub pending_table: &'static str,
    pub compare_lock: i64,
}

impl IndexSlot {
    /// The index a hub serves searches from.
    pub const SERVING: Self = Self {
        pending_table: "artifact_index_pending",
        compare_lock: BACKFILL_LOCK,
    };

    /// The index kept beside it in shadow mode and never searched by a
    /// user ([`MirrorStore`]): its own pending list, noted by the same
    /// trigger while noting for it is on (V029), and its own compare lock.
    pub const MIRROR: Self = Self {
        pending_table: "artifact_index_pending_mirror",
        compare_lock: MIRROR_COMPARE_LOCK,
    };
}

/// The lock one compare of the mirror index runs under: not
/// [`BACKFILL_LOCK`], so that the two indexes' compares never wait on
/// each other.
const MIRROR_COMPARE_LOCK: i64 = 0x6172_7469_6d69_7272;

/// The session lock the hubs serving an index of this kind hold, shared,
/// and a seeding of it asks for alone ([`KnowledgeStore::seed_index`]).
pub fn serving_lock(backend: &str) -> i64 {
    // FNV-1a of the name, under a prefix no other lock of the hub's has.
    let hash = backend
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    0x7366_0000_0000_0000 | (hash & 0xffff_ffff_ffff) as i64
}

/// A connection holding a session lock until it is dropped.
pub type HeldLock = deadpool_postgres::ClientWrapper;

/// Hold the serving locks of `backends`' artifact indexes alone, each on
/// a connection of its own, for as long as the returned connections are
/// kept (dropped, each is closed, and its lock goes with the session).
/// Returns those held and the kinds some hub serves now, whose keeper
/// holds the lock shared, as Postgres says. Promotion's proof that no hub
/// is running, kept to its end so that no hub's keeper takes the lock
/// meanwhile (DW-676). Like seeding's refusal, it proves nothing behind
/// PgBouncer in transaction mode, nor of a process that keeps no artifact
/// index.
pub async fn hold_unserved(
    pool: &PostgresPool,
    backends: &[&'static str],
) -> Result<(Vec<HeldLock>, Vec<&'static str>)> {
    let mut held = Vec::new();
    let mut served = Vec::new();
    for backend in backends {
        // Never handed back to the pool: closed when dropped.
        let alone = deadpool_postgres::Object::take(pool.get().await?);
        let free: bool = alone
            .query_one("SELECT pg_try_advisory_lock($1)", &[&serving_lock(backend)])
            .await?
            .get(0);
        match free {
            true => held.push(alone),
            false => served.push(*backend),
        }
    }
    Ok((held, served))
}

/// A hub is serving the index a seeding was asked for.
#[derive(Debug)]
pub struct SeedRefused(pub &'static str);

impl std::fmt::Display for SeedRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a hub is serving this {} index, and keeps it in line with Postgres itself: `workforce artifacts reindex` is for an index no hub serves. To have the hub fill its index again, drop the collection; to have it try at once what it is waiting to try again, POST /api/admin/artifacts/index/retry",
            self.0
        )
    }
}

impl std::error::Error for SeedRefused {}

/// How much of what the index said of an artifact it did not take is
/// kept with it.
const LAST_ERROR_KEPT: usize = 500;

/// How long an artifact the index did not take waits before its next try,
/// having been tried `tries` times: `pace.backoff`, doubled each time, up
/// to `pace.backoff_cap`; and a fifth more or less, by the artifact and
/// the try, so that those that failed together are not all due together.
/// The same for the same artifact and try, wherever it is worked out.
pub fn backoff(pace: &IndexPace, id: &str, tries: i32) -> std::time::Duration {
    workforce_common::backoff::jittered(pace.backoff, pace.backoff_cap, id, tries)
}

/// When it is, for everything the keeper of the index decides by time:
/// given to the store, so that a test can say.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// What the keeper does after a round of indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Another round: this one indexed something and more is due.
    AtOnce,
    /// Wait this long: the index took nothing this round.
    After(std::time::Duration),
    /// Nothing is due until then.
    At(DateTime<Utc>),
}

/// An artifact the index has not taken, tried again each time it is due:
/// how many times so far, since when, when next, and what the index last
/// said. Nothing is ever given up: it is tried for as long as it is
/// pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failing {
    pub id: String,
    pub tries: i32,
    pub first_failed_at: Option<DateTime<Utc>>,
    pub next_try_at: Option<DateTime<Utc>>,
    pub error: String,
}

/// What working the pending list once came to.
#[derive(Debug, Default)]
struct Worked {
    /// Artifacts given to the index, and how many it took of them:
    /// indexed, or removed for being no longer stored.
    tried: usize,
    indexed: usize,
    removed: usize,
}

impl Worked {
    fn taken(&self) -> usize {
        self.indexed + self.removed
    }
}

/// What became of one try at one artifact.
enum Tried {
    Indexed,
    Removed,
    /// The index did not take it, or not within the keeper's deadline: it
    /// is noted afresh, to be tried again.
    Failed,
    /// Nothing was pending of it by the time it was looked at.
    NotPending,
}

/// An artifact a pass could not index, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexFailure {
    pub id: String,
    pub error: String,
}

/// What a repair of the index did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Backfilled {
    /// Artifacts indexed: those the index lacked or held another text of.
    pub indexed: usize,
    /// Artifacts and vectors the index and Postgres disagreed about.
    pub total: usize,
    /// Vectors removed whose artifact is no longer stored.
    pub removed: usize,
    /// Artifacts still pending once the pending list was worked: the
    /// index did not take them, and they are tried again in their time.
    /// The first twenty failing are in `failures`, with why.
    pub failed: usize,
    pub failures: Vec<IndexFailure>,
}

/// How a repair running now is getting on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BackfillProgress {
    pub indexed: usize,
    pub total: usize,
}

/// The state of the artifact index, for health reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexState {
    /// Whether an index is configured at all; without one, search goes by
    /// keyword and the rest says nothing.
    pub configured: bool,
    /// Whether the index holds every artifact, so that search relies on
    /// it, as last looked at; `None` before the first look, or when the
    /// last could not tell (see `last_error`).
    pub complete: Option<bool>,
    /// A repair running now.
    pub backfill: Option<BackfillProgress>,
    /// What the last repair that found something to do did.
    pub last_backfill: Option<Backfilled>,
    /// When the index was last compared with Postgres artifact by artifact.
    pub reconciled_at: Option<DateTime<Utc>>,
    /// Artifacts written or deleted that the index has yet to take.
    pub pending: usize,
    /// The pending artifacts the index has failed to take, each tried
    /// again when it is due.
    pub failing: Vec<Failing>,
    /// How many of those wait the longest between tries, a quarter of an
    /// hour: they have failed at every try for a while.
    pub failing_at_cap: usize,
    /// When the one failing longest first failed.
    pub oldest_failure_at: Option<DateTime<Utc>>,
    /// How a search goes as things stood at the keeper's last round:
    /// `"index"`, or `"keyword"` while the index is not marked compared
    /// or more is pending than a search by it may leave out.
    pub search: Option<&'static str>,
    /// When the keeper last counted what is pending: what the counts here
    /// are as of.
    pub counted_at: Option<DateTime<Utc>>,
    /// Why the index is not the one the hub made, if a write found it so
    /// ([`ArtifactIndexer::refused_index`]): searched by keyword meanwhile.
    pub not_ours: Option<String>,
    /// Whether the index's count was in step with Postgres at the
    /// keeper's last reading of it ([`in_step`]); `None` before one.
    pub in_step: Option<bool>,
    /// Why the last round left something undone.
    pub last_error: Option<String>,
    /// Whether searches are asking the index (`"closed"`), leaving it
    /// alone after it kept not answering in time (`"open"`), or about to
    /// try it again (`"half_open"`); `None` where searches are given no
    /// deadline.
    pub search_breaker: Option<&'static str>,
}

/// What the store shares with the task that keeps its index complete.
#[derive(Default)]
struct IndexKeeping {
    state: std::sync::Mutex<IndexState>,
    /// Wakes the keeper before its watch is over.
    wake: tokio::sync::Notify,
    /// The pace the keeper was started at: what a write's own try at the
    /// index goes by too.
    pace: std::sync::Mutex<IndexPace>,
    /// The last compare this process ran to its end, and how it ended.
    compared_at: std::sync::Mutex<Option<Compared>>,
    /// Whether this store's keeper is running ([`KeeperAlive`]).
    alive: std::sync::atomic::AtomicBool,
    /// Whether it is making progress ([`Progress`]).
    progress: std::sync::Mutex<Progress>,
}

/// A keeper's progress: when it last made some, and by when it is to make
/// more, a stall bound past each step or past a wait it chose. Alive and
/// stalled (a statement or a call that never returns) is as bad as dead:
/// the mirror's lease is renewed only while a keeper makes progress.
#[derive(Debug, Default, Clone, Copy)]
struct Progress {
    at: Option<std::time::Instant>,
    expect_by: Option<std::time::Instant>,
    /// How long one step may take before the keeper counts as stalled.
    stall: std::time::Duration,
}

impl IndexKeeping {
    /// The keeper made progress just now.
    fn progressed(&self) {
        let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        progress.at = Some(now);
        progress.expect_by = Some(now + progress.stall);
    }

    /// The keeper waits `wait` by its own choice before its next round.
    fn waiting(&self, wait: std::time::Duration) {
        let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        progress.expect_by = Some(std::time::Instant::now() + wait + progress.stall);
    }

    /// Whether the keeper is past when it was to make progress.
    fn stalled(&self) -> bool {
        let progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        progress
            .expect_by
            .is_some_and(|by| std::time::Instant::now() > by)
    }

    /// How long ago the keeper last made progress.
    fn progress_age(&self) -> Option<std::time::Duration> {
        let progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        progress.at.map(|at| at.elapsed())
    }
}

/// Held by a keeper's task: marks the keeper alive when made and dead when
/// dropped, as when the task ends, or unwinds from a panic.
struct KeeperAlive(Arc<IndexKeeping>);

impl KeeperAlive {
    fn new(keeping: Arc<IndexKeeping>) -> Self {
        keeping
            .alive
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Self(keeping)
    }
}

impl Drop for KeeperAlive {
    fn drop(&mut self) {
        self.0
            .alive
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A compare run to its end, as [`IndexKeeping::compared_at`] has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Compared {
    /// When it ended, by the store's clock.
    at: DateTime<Utc>,
    /// Whether it ended without error and left the count in step with
    /// Postgres, or the count has been read in step since. Until then no
    /// compare is started for a count out of step or a lost marker within
    /// `backoff_cap` of `at`: one that could not put the index right is
    /// not run again at once, round after round.
    ended_well: bool,
}

/// How long ago `at` was, at `now`. A time after `now`, as a clock set
/// back leaves behind, counts as long ago: nothing waits on the clock's
/// step.
fn since(now: DateTime<Utc>, at: DateTime<Utc>) -> std::time::Duration {
    (now - at).to_std().unwrap_or(std::time::Duration::MAX)
}

/// How long the keeper waits, at `now`, for a round it was told to hold at
/// `next`: not past the next reconcile of an index compared by its text,
/// nor past the end of the hold after a compare that ended badly.
fn keeper_wait(
    next: DateTime<Utc>,
    now: DateTime<Utc>,
    compared: Option<Compared>,
    pace: &IndexPace,
    compares_text: bool,
) -> std::time::Duration {
    let mut wait = (next - now).to_std().unwrap_or_default();
    if let Some(last) = compared {
        let ago = since(now, last.at);
        if compares_text {
            wait = wait.min(pace.reconcile.saturating_sub(ago));
        }
        if !last.ended_well {
            wait = wait.min(pace.backoff_cap.saturating_sub(ago));
        }
    }
    wait
}

/// One reading of the index's count beside Postgres's.
#[derive(Debug, Clone, Copy)]
struct Reading {
    held: usize,
    stored: usize,
    pending: usize,
}

impl Reading {
    /// Whether the count is in step with Postgres ([`in_step`]).
    fn in_step(&self) -> bool {
        in_step(self.held, self.stored, self.pending)
    }
}

impl IndexKeeping {
    fn pace(&self) -> IndexPace {
        *self.pace.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn update(&self, change: impl FnOnce(&mut IndexState)) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        change(&mut state);
    }
}

/// Another repair holds the lock: one at a time indexes the artifacts.
#[derive(Debug)]
pub struct BackfillBusy;

impl std::fmt::Display for BackfillBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("another process is indexing the artifacts")
    }
}

impl std::error::Error for BackfillBusy {}

/// Holds [`BACKFILL_LOCK`] on a connection of its own, taken out of the
/// pool. A repair that ends lets it go ([`Self::release`]) before the
/// connection is dropped: Postgres frees a session's locks when it notices
/// the session gone, which is a moment after the socket closes, and a
/// repair asked for in that moment would find the last one still at it.
/// However the repair ends, dropping this closes the connection, and the
/// lock goes with the session.
struct BackfillLock(Arc<deadpool_postgres::ClientWrapper>, i64);

#[cfg(test)]
tokio::task_local! {
    /// The connections of the locks taken in a test's scope, kept open
    /// after their locks are dropped: the moment in which Postgres has not
    /// yet noticed a closed session, made to last.
    static KEPT_OPEN: Arc<std::sync::Mutex<Vec<Arc<deadpool_postgres::ClientWrapper>>>>;
}

impl BackfillLock {
    /// The lock, waited for when `wait`; otherwise [`BackfillBusy`] if it
    /// is held.
    async fn acquire(pool: &PostgresPool, key: i64, wait: bool) -> Result<Self> {
        let conn = deadpool_postgres::Object::take(pool.get().await?);
        if wait {
            conn.execute("SELECT pg_advisory_lock($1)", &[&key]).await?;
        } else {
            let held: bool = conn
                .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
                .await?
                .get(0);
            if !held {
                return Err(eyre::Report::new(BackfillBusy));
            }
        }
        let conn = Arc::new(conn);
        #[cfg(test)]
        let _ = KEPT_OPEN.try_with(|kept| kept.lock().unwrap().push(Arc::clone(&conn)));
        Ok(Self(conn, key))
    }

    /// Let the lock go now, rather than with the connection.
    async fn release(self) {
        let _ = self
            .0
            .execute("SELECT pg_advisory_unlock($1)", &[&self.1])
            .await;
    }

    /// `Err` once the lock is lost: it goes with its connection, and
    /// another process may then be repairing the index too.
    async fn held(&self) -> Result<()> {
        self.0
            .query_one("SELECT 1", &[])
            .await
            .map(|_| ())
            .wrap_err(
                "the connection holding the artifact index lock was lost, and the lock with it",
            )
    }
}

/// Storage abstraction shared by the Postgres and file backends.
#[async_trait]
pub trait ArtifactBackend: Send + Sync {
    /// Create a new artifact. Errors if the id already exists.
    async fn create(&self, artifact: &Artifact) -> Result<()>;
    /// Read by id, applying read ACL. Returns `None` if missing or not
    /// visible to `caller`.
    async fn read(&self, id: &str, caller: &Caller) -> Result<Option<Artifact>>;
    /// Replace content (owner only; blocked once finalized). Returns the
    /// updated artifact.
    async fn update(
        &self,
        id: &str,
        content: &str,
        modified_by: &str,
        caller: &Caller,
    ) -> Result<Artifact>;
    /// Lock an artifact against further edits (owner only).
    async fn finalize(&self, id: &str, caller: &Caller) -> Result<Artifact>;
    /// Hard-delete an artifact (owner only).
    async fn delete(&self, id: &str, caller: &Caller) -> Result<()>;
    /// List artifacts visible to `caller`, newest first.
    async fn list(&self, caller: &Caller, filter: &ArtifactFilter) -> Result<Vec<Artifact>>;
    /// Change an artifact's visibility scope (owner only).
    async fn set_visibility(&self, id: &str, visibility: Visibility, caller: &Caller)
        -> Result<()>;
    /// Grant a specific user read access to a private artifact (owner only).
    async fn share_with(&self, id: &str, user_id: &str, caller: &Caller) -> Result<()>;
    /// Search artifacts visible to `caller`. Semantic when an indexer is
    /// wired, otherwise a substring match.
    async fn search(
        &self,
        query: &str,
        caller: &Caller,
        filter: &ArtifactFilter,
        limit: usize,
    ) -> Result<Vec<ScoredArtifact>>;
    /// As [`Self::search`], saying whether the hits were ranked by an
    /// index or matched by keyword, and why. A backend with no index
    /// matches by keyword.
    async fn search_told(
        &self,
        query: &str,
        caller: &Caller,
        filter: &ArtifactFilter,
        limit: usize,
    ) -> Result<Searched> {
        Ok(Searched {
            hits: self.search(query, caller, filter, limit).await?,
            by_keyword: Some(Skipped::NoIndex),
            not_indexed: 0,
        })
    }
    /// Hand an artifact to another owner (an identity lifecycle operation;
    /// no caller check). Its grants stay, except one naming the new owner.
    async fn set_owner(&self, id: &str, owner: &str) -> Result<()>;
    /// The users an artifact is shared with by name.
    async fn shares_of(&self, id: &str) -> Result<Vec<String>>;
    /// The artifacts shared with `user` by name.
    async fn grants_of(&self, user: &str) -> Result<Vec<String>>;
    /// Take `user` off an artifact's grants; `true` when one went.
    async fn revoke_share(&self, id: &str, user: &str) -> Result<bool>;
}

const SELECT_COLS: &str = "id, artifact_type, title, content, owner_id, visibility, tags, \
     status, version, created_by, last_modified_by, created_at, updated_at";

/// Postgres-backed artifact store (the production backend).
///
/// Lives in the `telemetry` schema; its tables ship in
/// `V005__artifacts.sql`. The supplied pool must pin `search_path` to that
/// schema — use [`KnowledgeStore::build_pool`].
pub struct KnowledgeStore {
    pool: PostgresPool,
    indexer: Option<Arc<dyn ArtifactIndexer>>,
    index: Arc<IndexKeeping>,
    /// How long a search waits for the index, if it does not wait for as
    /// long as the index takes.
    search: Option<Arc<SearchWait>>,
    clock: Clock,
    slot: IndexSlot,
    /// The mirror's keeping, when this store has a mirror
    /// ([`KnowledgeStore::open_mirror`]): every write here wakes its
    /// keeper, which would otherwise wait out its watch.
    mirror_wake: std::sync::OnceLock<Arc<IndexKeeping>>,
    /// Shadow mode's reads, when set ([`KnowledgeStore::shadow_reads`]).
    read_shadow: std::sync::OnceLock<ReadShadow>,
}

/// Shadow mode's reads of the artifact index (DW-676): each search this
/// store's index ranks by meaning is asked of the mirror too, in the
/// background, and the two rankings compared.
struct ReadShadow {
    shadow: Arc<workforce_common::shadow::Shadow>,
    /// The mirror's index, as its reads are made: one attempt each.
    reader: Arc<dyn ArtifactIndexer>,
    /// The mirror's keeping, whose last state says whether it is in step.
    mirror: Arc<IndexKeeping>,
}

impl ReadShadow {
    /// Whether the mirror could be relied on as its keeper last saw it:
    /// marked compared, no more pending than a search may leave out, and
    /// its count not found out of step. Read from memory, never asked.
    fn mirror_in_step(&self) -> bool {
        let state = self.mirror.state.lock().unwrap_or_else(|e| e.into_inner());
        state.search == Some("index") && state.in_step != Some(false)
    }
}

/// The artifact index kept beside the serving one in shadow mode
/// (DW-676), from its own pending list ([`IndexSlot::MIRROR`]) and with
/// its own keeping. Not an [`ArtifactBackend`]: nothing is written through
/// it, so it never tags, and no user search reaches it.
pub struct MirrorStore(Arc<KnowledgeStore>, Arc<MirrorLease>);

/// How many of the mirror keeper's watches one renewal of the lease lasts.
pub const LEASE_WATCHES: u32 = 3;
/// Connections a mirror's own pool holds ([`KnowledgeStore::build_mirror_pool`]).
pub const MIRROR_POOL_SIZE: usize = 3;
/// How long one of a mirror keeper's statements, or its wait for a lock,
/// may take before Postgres gives it up.
pub const MIRROR_STATEMENT_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The lease under which writes are noted for the mirror, as this
/// process last saw it.
#[derive(Debug, Default)]
pub struct MirrorLease {
    state: std::sync::Mutex<LeaseState>,
}

/// What [`MirrorLease`] tells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeaseState {
    /// When the lease ends, as last renewed here.
    pub until: Option<DateTime<Utc>>,
    /// How many times a renewal here found it lapsed after this process
    /// had held it: writes made meanwhile went un-noted, for the next
    /// compare to find.
    pub lapses: u64,
    /// Whether the mirror's keeper is running. A keeper that ended, as by
    /// a panic, leaves the lease to lapse within one span.
    pub keeper_alive: bool,
    /// Whether it is running and stuck: past when it was to make progress.
    /// Its lease is not renewed meanwhile.
    pub keeper_stalled: bool,
    /// How long ago it last made progress.
    pub progress_age: Option<std::time::Duration>,
}

/// What a renewal of the lease found and left, both on the database's
/// clock (`artifact_mirror_renew`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Renewal {
    /// Whether the lease had lapsed: never held, ended, or past.
    pub had_lapsed: bool,
    /// When it ends now.
    pub until: DateTime<Utc>,
}

impl MirrorLease {
    /// Note a renewal: whether the lease had lapsed after this process
    /// had held it, counted if so.
    fn renewed(&self, renewal: Renewal) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let lapsed = state.until.is_some() && renewal.had_lapsed;
        if lapsed {
            state.lapses += 1;
        }
        state.until = Some(renewal.until);
        lapsed
    }

    pub fn state(&self) -> LeaseState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl MirrorStore {
    /// The mirror over `pool`, by `indexer`, as `workforce artifacts
    /// reindex --mirror` seeds it: the lease left as it is.
    pub async fn seeding(pool: PostgresPool, indexer: Arc<dyn ArtifactIndexer>) -> Result<Self> {
        let store = KnowledgeStore::made_in(
            pool,
            Some(indexer),
            None,
            Arc::new(Utc::now),
            IndexSlot::MIRROR,
        )
        .await?;
        Ok(Self(store, Arc::default()))
    }

    /// Keep the mirror complete in the background, as
    /// [`KnowledgeStore::keep_index_complete`] keeps the serving index,
    /// and renew the lease every `pace.watch` for [`LEASE_WATCHES`]
    /// watches. The renewal has a task of its own and not a step in the
    /// keeper's rounds: a round (a full compare) can outlast the lease.
    /// A renewal that finds the lease lapsed says so; health counts it.
    pub fn keep_index_complete(&self, pace: IndexPace) {
        self.0.keep_index_complete(pace);
        let store = Arc::downgrade(&self.0);
        let lease = Arc::clone(&self.1);
        let span = pace.watch * LEASE_WATCHES;
        tokio::spawn(async move {
            let mut stalled_said = false;
            loop {
                let Some(store) = store.upgrade() else {
                    return;
                };
                // Only while the keeper runs: a lease renewed for a keeper
                // that is gone would have writes noted with nobody to take
                // them, every sign of health frozen at its last round.
                if !store.index.alive.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::error!(
                        "the artifact mirror's keeper has stopped: its lease is no longer renewed, and lapses within {span:?}, so writes stop being noted for it"
                    );
                    return;
                }
                // Running and stuck is as bad as stopped: renewed again
                // once it moves on.
                if store.index.stalled() {
                    if !stalled_said {
                        tracing::error!(
                            progress_age = ?store.index.progress_age(),
                            "the artifact mirror's keeper has made no progress for longer than any step may take: its lease is not renewed while it is stuck, and lapses within {span:?}"
                        );
                        stalled_said = true;
                    }
                    drop(store);
                    tokio::time::sleep(pace.watch).await;
                    continue;
                }
                stalled_said = false;
                match store.renew_mirror_lease(span).await {
                    Ok(renewal) => {
                        if lease.renewed(renewal) {
                            tracing::warn!(
                                "the artifact mirror's lease had lapsed: writes made meanwhile were not noted for it; it is compared now to find them"
                            );
                            // Compared at its next round, woken now: a
                            // lapse costs seconds, not the hour to the next
                            // reconcile.
                            *store
                                .index
                                .compared_at
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = None;
                            store.index.wake.notify_one();
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "the artifact mirror's lease was not renewed")
                    }
                }
                drop(store);
                tokio::time::sleep(pace.watch).await;
            }
        });
    }

    /// Fill the mirror while no hub keeps it
    /// ([`KnowledgeStore::seed_index`]).
    pub async fn seed_index(&self, progress: impl Fn(usize, usize)) -> Result<Backfilled> {
        self.0.seed_index(progress).await
    }

    /// The mirror's state, as its keeper last knew it.
    pub fn index_state(&self) -> IndexState {
        self.0.index_state()
    }

    /// The lease, as this process last renewed it, and whether the
    /// mirror's keeper is running.
    pub fn lease(&self) -> LeaseState {
        LeaseState {
            keeper_alive: self.0.index.alive.load(std::sync::atomic::Ordering::SeqCst),
            keeper_stalled: self.0.index.stalled(),
            progress_age: self.0.index.progress_age(),
            ..self.1.state()
        }
    }
}

/// How long a search waits for the index before it goes by keyword, and
/// the breaker that leaves an index alone that keeps not answering in
/// time.
struct SearchWait {
    deadline: std::time::Duration,
    breaker: workforce_common::breaker::Breaker,
}

/// Why a search did not go by the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    /// No index is configured.
    NoIndex,
    /// The index does not hold every artifact, or cannot be relied on.
    Incomplete,
    /// The index failed at once.
    Failed(String),
    /// The index did not answer within the search's deadline.
    TimedOut(std::time::Duration),
    /// The index is being left alone, having not answered in time lately.
    Resting,
    /// More artifacts are waiting to be indexed than a search by the
    /// index may leave out: so many of so many stored.
    Behind { pending: usize, stored: usize },
    /// The index is not the one the hub made, as a write found: why.
    NotOurs(String),
    /// The index holds a number of artifacts it could not hold in step
    /// with Postgres ([`in_step`]): lost, emptied, or made again behind
    /// the hub.
    OutOfStep {
        held: usize,
        stored: usize,
        pending: usize,
    },
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoIndex => write!(f, "no index of artifacts is configured"),
            Self::Incomplete => write!(f, "the index does not hold every artifact yet"),
            Self::Failed(why) => write!(f, "the index failed: {why}"),
            Self::TimedOut(deadline) => {
                write!(f, "the index did not answer within {deadline:?}")
            }
            Self::NotOurs(why) => write!(f, "the index is not the one the hub made: {why}"),
            Self::OutOfStep {
                held,
                stored,
                pending,
            } => write!(
                f,
                "the index holds {held} artifacts, which it could not if it were in step with the {stored} stored and the {pending} waiting to be indexed"
            ),
            Self::Behind { pending, stored } => write!(
                f,
                "{pending} of the {stored} artifacts stored are not indexed yet"
            ),
            Self::Resting => write!(
                f,
                "the index is being left alone for a while, having not answered in time"
            ),
        }
    }
}

/// What a search found, and how it searched.
#[derive(Debug, Clone)]
pub struct Searched {
    pub hits: Vec<ScoredArtifact>,
    /// `None` when the index ranked the hits by meaning; otherwise why it
    /// did not, the hits being those whose text holds the query.
    pub by_keyword: Option<Skipped>,
    /// When the index ranked the hits: how many artifacts, at least, it
    /// does not hold as they are stored, and so could not find.
    pub not_indexed: usize,
}

/// What the index answered a search.
enum Asked {
    /// What it found, and how many artifacts it holds.
    Ranked {
        ranked: Vec<(String, f32)>,
        held: usize,
    },
    /// It is not recorded complete, before the search or after it: one
    /// found lost by the very search that asks it is made again, empty.
    Incomplete,
    /// It is not the index the hub made ([`ArtifactIndexer::refused_index`]).
    Refused(String),
}

/// What the index finds for `query`, if it can be relied on. Everything
/// asked here is asked of the index, and nothing of Postgres: this is what
/// a search's deadline times. `Err` when the index does not answer.
async fn ask_index(idx: &dyn ArtifactIndexer, query: &str, limit: usize) -> Result<Asked> {
    if !idx.complete().await? {
        return Ok(Asked::Incomplete);
    }
    if let Some(why) = idx.refused_index() {
        return Ok(Asked::Refused(why));
    }
    // Over-fetch so post-ACL/filtering can still reach `limit`. How many
    // it holds is asked with the question: a search is answered only from
    // an index in step with Postgres.
    let wanted = limit.saturating_mul(4).max(limit);
    let (ranked, held) = idx.search_counted(query, wanted).await?;
    if !idx.complete().await? {
        return Ok(Asked::Incomplete);
    }
    Ok(Asked::Ranked { ranked, held })
}

/// Whether an index that holds `held` artifacts can be in step with
/// Postgres, which stores `stored` and has `pending` noted as not yet
/// indexed as stored: each pending one explains at most one missing from
/// the index (not yet indexed) or one too many (not yet removed), so an
/// index in step holds between `stored - pending` and `stored + pending`.
/// Outside that it is not what Postgres believes. Inside it, it may still
/// be: as many missing as extra pass, and so does a loss of no more than
/// `pending`.
pub fn in_step(held: usize, stored: usize, pending: usize) -> bool {
    held + pending >= stored && held <= stored + pending
}

/// How many pending artifacts a search by the index may leave out, of so
/// many stored: ten, or one in a hundred if that is more. Beyond it the
/// search goes by keyword, which finds them.
pub fn search_tolerance(stored: usize) -> usize {
    (stored / 100).max(10)
}

/// Timeouts in a row before the index is left alone.
pub const SEARCH_TRIPS_AFTER: u32 = 3;

impl KnowledgeStore {
    /// Build a telemetry-pinned pool suitable for the artifact tables.
    pub fn build_pool(
        config: &workforce_common::postgres::PostgresConfig,
    ) -> Result<PostgresPool, workforce_common::postgres::PostgresError> {
        workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool(config)
    }

    /// Ensure the artifact tables exist, then return a ready store.
    pub async fn new(
        pool: PostgresPool,
        indexer: Option<Arc<dyn ArtifactIndexer>>,
    ) -> Result<Arc<Self>> {
        Self::made(pool, indexer, None, Arc::new(Utc::now)).await
    }

    /// As [`Self::new`], a search waiting `search_deadline` for the index
    /// before it goes by keyword. After [`SEARCH_TRIPS_AFTER`] searches in
    /// a row have waited that out, the index is left alone for
    /// `search_rest`, and then tried by one search at a time until it
    /// answers. [`Self::new`] sets no deadline: a search waits as long as
    /// the index's own client does.
    pub async fn waiting(
        pool: PostgresPool,
        indexer: Option<Arc<dyn ArtifactIndexer>>,
        search_deadline: std::time::Duration,
        search_rest: std::time::Duration,
    ) -> Result<Arc<Self>> {
        let wait = SearchWait {
            deadline: search_deadline,
            breaker: workforce_common::breaker::Breaker::new(SEARCH_TRIPS_AFTER, search_rest),
        };
        Self::made(pool, indexer, Some(Arc::new(wait)), Arc::new(Utc::now)).await
    }

    async fn made(
        pool: PostgresPool,
        indexer: Option<Arc<dyn ArtifactIndexer>>,
        search: Option<Arc<SearchWait>>,
        clock: Clock,
    ) -> Result<Arc<Self>> {
        Self::made_in(pool, indexer, search, clock, IndexSlot::SERVING).await
    }

    async fn made_in(
        pool: PostgresPool,
        indexer: Option<Arc<dyn ArtifactIndexer>>,
        search: Option<Arc<SearchWait>>,
        clock: Clock,
        slot: IndexSlot,
    ) -> Result<Arc<Self>> {
        workforce_common::telemetry_postgres::run_telemetry_migrations(&pool)
            .await
            .context("ensuring artifact tables exist")?;
        let index = Arc::new(IndexKeeping::default());
        index.update(|state| state.configured = indexer.is_some());
        Ok(Arc::new(Self {
            pool,
            indexer,
            index,
            search,
            clock,
            slot,
            mirror_wake: std::sync::OnceLock::new(),
            read_shadow: std::sync::OnceLock::new(),
        }))
    }

    /// Ask `mirror` too, in the background, of every search this store's
    /// index ranks by meaning while the mirror is in step: through
    /// `reader`, the mirror's index as shadow reads make one attempt, and
    /// `shadow`, the kernel that decides whether to ask and records what
    /// each side found. Both sides are timed end to end, each embedding
    /// the question by its own model. Nothing of this is awaited by a
    /// search, nothing of the serving index's keeping is changed, and no
    /// artifact is read from Postgres for it.
    pub fn shadow_reads(
        &self,
        mirror: &MirrorStore,
        reader: Arc<dyn ArtifactIndexer>,
        shadow: Arc<workforce_common::shadow::Shadow>,
    ) -> Result<()> {
        let read = ReadShadow {
            shadow,
            reader,
            mirror: Arc::clone(&mirror.0.index),
        };
        if self.read_shadow.set(read).is_err() {
            eyre::bail!("this artifact store's reads are already shadowed");
        }
        Ok(())
    }

    /// The primary's ranking of `query` by meaning, `took` to come, handed
    /// to the read shadow if there is one and the mirror is in step.
    fn observe_read(
        &self,
        query: &str,
        limit: usize,
        ranked: &[(String, f32)],
        took: std::time::Duration,
    ) {
        use workforce_common::shadow::{Observed, Ranked};
        let Some(read) = self.read_shadow.get() else {
            return;
        };
        if !read.mirror_in_step() {
            return;
        }
        let primary = Observed {
            ranked: Ranked {
                ids: ranked.iter().map(|(id, _)| id.clone()).collect(),
                top_score: ranked.first().map(|(_, score)| *score),
                embed: None,
            },
            latency: took,
            query_hash: Some(blake3::hash(query.as_bytes()).to_hex().to_string()),
            labels: vec![
                ("same_model", "false".to_string()),
                ("mirror_in_step", "true".to_string()),
            ],
        };
        let reader = Arc::clone(&read.reader);
        let query = query.to_string();
        // As many as the primary was asked for ([`ask_index`]).
        let wanted = limit.saturating_mul(4).max(limit);
        read.shadow.observe(primary, move || async move {
            let (found, _) = reader.search_counted(&query, wanted).await?;
            Ok(Ranked {
                top_score: found.first().map(|(_, score)| *score),
                ids: found.into_iter().map(|(id, _)| id).collect(),
                embed: None,
            })
        });
    }

    /// Renew the lease under which artifact writes are noted for the
    /// mirror index, for every hub on this Postgres, to at least now and
    /// `span` (`artifact_mirror_renew`, V029). Returns the lease as it was:
    /// `None` if never held or ended. Refused while the mirror's pending
    /// list is missing.
    pub async fn renew_mirror_lease(&self, span: std::time::Duration) -> Result<Renewal> {
        // Bounded, so that a renewal stuck on a lock or a slow Postgres
        // fails, is said, and is tried again at the next tick, rather than
        // holding the ticker: given up by Postgres after a third of the
        // span (at most ten seconds), and by the client after twice that.
        let limit = (span / LEASE_WATCHES)
            .max(std::time::Duration::from_secs(1))
            .min(std::time::Duration::from_secs(10));
        let ms = limit.as_millis().max(1);
        let renew = async {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            tx.batch_execute(&format!(
                "SET LOCAL statement_timeout = {ms}; SET LOCAL lock_timeout = {ms}"
            ))
            .await?;
            let span = format!("{} milliseconds", span.as_millis());
            let row = tx
                .query_one(
                    "SELECT had_lapsed, held_until FROM artifact_mirror_renew($1::text::interval)",
                    &[&span],
                )
                .await?;
            tx.commit().await?;
            eyre::Ok(Renewal {
                had_lapsed: row.get(0),
                until: row.get(1),
            })
        };
        tokio::time::timeout(limit * 2 + std::time::Duration::from_secs(1), renew)
            .await
            .map_err(|_| eyre::eyre!("Postgres did not answer within {:?}", limit * 2))?
            .context("renewing the lease under which artifact writes are noted for the mirror")
    }

    /// The pool a mirror's keeper works over ([`Self::open_mirror`]): of its
    /// own, three connections (the keeper's held serving lock, its
    /// statements, and the lease's renewal), every statement and lock wait
    /// given up by Postgres after `limit`. A statement stuck on a lock,
    /// held by a session left open, fails the round, and the next round
    /// tries again: a keeper is never held by one for ever.
    pub fn build_mirror_pool(
        config: &workforce_common::postgres::PostgresConfig,
        limit: std::time::Duration,
    ) -> Result<PostgresPool, workforce_common::postgres::PostgresError> {
        let ms = limit.as_millis().max(1);
        let statement = format!("SET statement_timeout = {ms}");
        let lock = format!("SET lock_timeout = {ms}");
        PostgresPool::from_config_with_session_init(
            &workforce_common::postgres::PostgresConfig {
                pool_size: MIRROR_POOL_SIZE,
                ..config.clone()
            },
            &["SET search_path TO telemetry, public", &statement, &lock],
        )
    }

    /// The mirror index kept beside this one in shadow mode, by `indexer`,
    /// over `pool` (bounded, [`Self::build_mirror_pool`]):
    /// writes noted for it under a lease taken now for `lease` (its keeper
    /// renews it, [`MirrorStore::keep_index_complete`]), and every write
    /// through this store waking its keeper.
    pub async fn open_mirror(
        &self,
        pool: PostgresPool,
        indexer: Arc<dyn ArtifactIndexer>,
        lease: std::time::Duration,
    ) -> Result<MirrorStore> {
        let mirror = MirrorStore::seeding(pool, indexer).await?;
        mirror.1.renewed(mirror.0.renew_mirror_lease(lease).await?);
        if self.mirror_wake.set(Arc::clone(&mirror.0.index)).is_err() {
            eyre::bail!("this artifact store already has a mirror");
        }
        Ok(mirror)
    }

    /// Wake the mirror's keeper, if this store has a mirror: a write has
    /// just been noted for it.
    fn wake_mirror(&self) {
        if let Some(mirror) = self.mirror_wake.get() {
            mirror.wake.notify_one();
        }
    }

    /// The state of the artifact index, as the store last knew it.
    pub fn index_state(&self) -> IndexState {
        let state = self.index.state.lock().unwrap_or_else(|e| e.into_inner());
        IndexState {
            search_breaker: self.search.as_ref().map(|wait| wait.breaker.told()),
            not_ours: self.indexer.as_ref().and_then(|idx| idx.refused_index()),
            ..state.clone()
        }
    }

    /// An artifact as stored, with its text hash ([`TEXT_HASH`]).
    async fn load_hashed(&self, id: &str) -> Result<Option<(Artifact, String)>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!(
                    "SELECT {SELECT_COLS}, {TEXT_HASH} AS text_hash FROM artifacts WHERE id = $1"
                ),
                &[&id],
            )
            .await?;
        row.map(|row| Ok((row_to_artifact(&row)?, row.try_get("text_hash")?)))
            .transpose()
    }

    /// The text hash an artifact is stored with now; [`DELETED`] if it is
    /// not stored.
    async fn stored_hash(&self, id: &str) -> Result<String> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!("SELECT {TEXT_HASH} FROM artifacts WHERE id = $1"),
                &[&id],
            )
            .await?;
        Ok(row.map_or_else(|| DELETED.to_string(), |row| row.get(0)))
    }

    /// Bring the index in line with Postgres about one artifact: index it
    /// as it is stored, or remove it if it is not stored. Returns the text
    /// hash the index now agrees with; [`DELETED`] for an artifact removed.
    ///
    /// The row is read again after the index took it, and the artifact
    /// settled only once the two reads agree. Whoever changes an artifact
    /// writes Postgres first and the index after, so a change this did not
    /// see in its second read reaches the index after this did, and one it
    /// did see is indexed here: either way the index ends on the newer. A
    /// read just before the write alone would leave a window between the
    /// two, and going by `updated_at` would trust the clocks of every hub
    /// that stamps it, and miss the tags, which are written without it.
    async fn settle(&self, idx: &dyn ArtifactIndexer, id: &str) -> Result<String> {
        for _ in 0..SETTLE_TRIES {
            let hash = match self.load_hashed(id).await? {
                Some((artifact, hash)) => {
                    idx.reindex(&artifact, &hash).await?;
                    hash
                }
                None => {
                    idx.remove(id.to_string()).await?;
                    DELETED.to_string()
                }
            };
            if self.stored_hash(id).await? == hash {
                return Ok(hash);
            }
        }
        Err(eyre::eyre!(
            "artifact '{id}' kept changing while it was indexed"
        ))
    }

    /// One try at bringing the index in line with Postgres about one
    /// pending artifact: the only way the keeper writes to the index.
    ///
    /// The number of its noting is read first. The artifact is then
    /// settled ([`Self::settle`]: written, what is stored read again, and
    /// written again if that changed), within `pace.deadline` whatever the
    /// index's own client would wait.
    ///
    /// Taken, the note is removed if it still has the number that was
    /// read: one made since is another's, and stays.
    ///
    /// Not taken, for whatever reason, the artifact is noted afresh, under
    /// a new number: the write may have reached the index all the same
    /// and may land late, over a newer one that someone else has since
    /// indexed and taken off the list, and then only a note still there
    /// puts it right. If the note was still the one read, it is counted
    /// a try, with why, and its next try put off ([`backoff`]); a noting
    /// made meanwhile is left as it is, due at once.
    async fn try_one(
        &self,
        idx: &dyn ArtifactIndexer,
        id: &str,
        pace: &IndexPace,
    ) -> Result<Tried> {
        let table = self.slot.pending_table;
        let conn = self.pool.get().await?;
        let read = format!("SELECT noted_seq, tries FROM {table} WHERE artifact_id = $1");
        let Some(row) = conn.query_opt(&read, &[&id]).await? else {
            return Ok(Tried::NotPending);
        };
        let (seen, tries): (i64, i32) = (row.get(0), row.get(1));
        drop(conn);
        let settled = tokio::time::timeout(pace.deadline, self.settle(idx, id)).await;
        self.index.progressed();
        let failed = match settled {
            Ok(Ok(hash)) => {
                let conn = self.pool.get().await?;
                let taken =
                    format!("DELETE FROM {table} WHERE artifact_id = $1 AND noted_seq = $2");
                conn.execute(&taken, &[&id, &seen]).await?;
                return Ok(match hash == DELETED {
                    true => Tried::Removed,
                    false => Tried::Indexed,
                });
            }
            Ok(Err(e)) => e,
            Err(_) => eyre::eyre!(
                "the index did not take it within {:?}; what was sent may yet land",
                pace.deadline
            ),
        };
        tracing::warn!(id, tries = tries + 1, error = ?failed, "artifact not indexed; it is tried again");
        let now = (self.clock)();
        let next = now + backoff(pace, id, tries + 1);
        let said: String = format!("{failed:#}")
            .chars()
            .take(LAST_ERROR_KEPT)
            .collect();
        // The hash a row made here is given is of what is stored now.
        let noted_afresh = format!(
            "INSERT INTO {table} (artifact_id, text_hash) \
             VALUES ($1, coalesce((SELECT {TEXT_HASH} FROM artifacts WHERE id = $1), $6)) \
             ON CONFLICT (artifact_id) DO UPDATE SET \
                 noted_seq = nextval('artifact_index_noted_seq'), \
                 tries = CASE WHEN {table}.noted_seq = $2 THEN {table}.tries + 1 ELSE {table}.tries END, \
                 last_error = CASE WHEN {table}.noted_seq = $2 THEN $3 ELSE {table}.last_error END, \
                 first_failed_at = CASE WHEN {table}.noted_seq = $2 \
                     THEN coalesce({table}.first_failed_at, $4) ELSE {table}.first_failed_at END, \
                 next_try_at = CASE WHEN {table}.noted_seq = $2 THEN $5 ELSE {table}.next_try_at END"
        );
        let conn = self.pool.get().await?;
        conn.execute(&noted_afresh, &[&id, &seen, &said, &now, &next, &DELETED])
            .await?;
        Ok(Tried::Failed)
    }

    /// The pending artifacts the index has failed to take, those that
    /// have waited longest first.
    async fn failing(&self) -> Result<Vec<Failing>> {
        let table = self.slot.pending_table;
        let conn = self.pool.get().await?;
        let read = format!(
            "SELECT artifact_id, tries, first_failed_at, next_try_at, coalesce(last_error, '') \
             FROM {table} WHERE tries > 0 ORDER BY first_failed_at NULLS FIRST, artifact_id"
        );
        let rows = conn.query(&read, &[]).await?;
        let failing = rows.iter().map(|row| Failing {
            id: row.get(0),
            tries: row.get(1),
            first_failed_at: row.get(2),
            next_try_at: row.get(3),
            error: row.get(4),
        });
        Ok(failing.collect())
    }

    /// Work the pending artifacts that are due: those never yet failed
    /// first, then those due longest, each tried once ([`Self::try_one`]).
    /// One the index does not take does not end the round; after
    /// [`ROUND_ENDS_AFTER`] in a row it ends, the rest left as they are.
    async fn work_due(&self, idx: &dyn ArtifactIndexer, pace: &IndexPace) -> Result<Worked> {
        let table = self.slot.pending_table;
        let due = format!(
            "SELECT artifact_id FROM {table} \
             WHERE (next_try_at IS NULL OR next_try_at <= $1) AND NOT (artifact_id = ANY($2)) \
             ORDER BY (tries = 0) DESC, next_try_at NULLS FIRST, artifact_id LIMIT {PENDING_PAGE}"
        );
        let mut tried_this_round: Vec<String> = Vec::new();
        let mut worked = Worked::default();
        let mut in_a_row = 0;
        'round: loop {
            let now = (self.clock)();
            let page: Vec<String> = {
                let conn = self.pool.get().await?;
                let rows = conn.query(&due, &[&now, &tried_this_round]).await?;
                rows.iter().map(|row| row.get(0)).collect()
            };
            if page.is_empty() {
                break;
            }
            for id in page {
                match self.try_one(idx, &id, pace).await? {
                    Tried::Indexed => {
                        worked.indexed += 1;
                        in_a_row = 0;
                    }
                    Tried::Removed => {
                        worked.removed += 1;
                        in_a_row = 0;
                    }
                    Tried::Failed => in_a_row += 1,
                    Tried::NotPending => {
                        tried_this_round.push(id);
                        continue;
                    }
                }
                worked.tried += 1;
                tried_this_round.push(id);
                if in_a_row == ROUND_ENDS_AFTER {
                    break 'round;
                }
            }
        }
        Ok(worked)
    }

    /// The ids of the stored artifacts the index lacks or holds another
    /// text of, in id order: Postgres is gone through a page at a time,
    /// and the index asked of each page ([`ArtifactIndexer::differing`]).
    /// Nothing is asked of the pending list.
    async fn differing(&self, idx: &dyn ArtifactIndexer) -> Result<Vec<String>> {
        let mut differing = Vec::new();
        let mut after = String::new();
        loop {
            let page: Vec<(String, String)> = {
                let conn = self.pool.get().await?;
                let rows = conn
                    .query(
                        &format!(
                            "SELECT id, {TEXT_HASH} FROM artifacts WHERE id > $1 \
                             ORDER BY id LIMIT {BACKFILL_PAGE}"
                        ),
                        &[&after],
                    )
                    .await?;
                rows.iter().map(|row| (row.get(0), row.get(1))).collect()
            };
            let Some((last, _)) = page.last() else { break };
            after = last.clone();
            differing.extend(idx.differing(&page).await?);
            self.index.progressed();
            if page.len() < BACKFILL_PAGE as usize {
                break;
            }
        }
        differing.sort();
        Ok(differing)
    }

    /// The ids the index holds of artifacts that are not stored: read
    /// through the index's ids from its start to its end, a page at a
    /// time, each page checked against Postgres. Only called for when the
    /// index holds more than is stored.
    async fn strays(&self, idx: &dyn ArtifactIndexer) -> Result<Vec<String>> {
        let mut strays = Vec::new();
        let mut after = None;
        loop {
            let (ids, next) = idx.ids_after(after, PENDING_PAGE as usize).await?;
            self.index.progressed();
            if !ids.is_empty() {
                let conn = self.pool.get().await?;
                let rows = conn
                    .query("SELECT id FROM artifacts WHERE id = ANY($1)", &[&ids])
                    .await?;
                let stored: BTreeSet<String> = rows.iter().map(|row| row.get(0)).collect();
                strays.extend(ids.into_iter().filter(|id| !stored.contains(id)));
            }
            after = next;
            if after.is_none() {
                return Ok(strays);
            }
        }
    }

    /// Compare the index with Postgres artifact by artifact, by id and
    /// text hash, note what differs (the compare), and work the
    /// pending list once, reporting `(done, total)`. An index made new is
    /// filled this way, all of it differing. What the index does not take
    /// stays pending and is in what is returned; it is tried again by
    /// whoever keeps the index.
    ///
    /// One compare runs at a time across every process on the database:
    /// with `wait`, this one waits its turn, and whatever is waiting to be
    /// tried again is tried now; otherwise it is [`BackfillBusy`]. `Err`
    /// too with no index, saying so.
    pub async fn backfill(
        &self,
        wait: bool,
        progress: impl Fn(usize, usize),
    ) -> Result<Backfilled> {
        let idx = self.indexer.as_deref().ok_or_else(|| {
            eyre::eyre!("no artifact index is configured: artifacts are found by keyword only")
        })?;
        let one_at_a_time = BackfillLock::acquire(&self.pool, self.slot.compare_lock, wait).await?;
        if wait {
            self.retry_now().await?;
        }
        let pace = self.index.pace();
        let done = self
            .compared_and_worked(idx, &one_at_a_time, &pace, progress)
            .await;
        one_at_a_time.release().await;
        match &done {
            Ok(done) => self.compared(done, true),
            Err(e) => self.index.update(|state| {
                state.backfill = None;
                state.last_error = Some(format!("{e:#}"));
            }),
        }
        done
    }

    /// Compare, and index what was noted and whatever was pending before.
    /// Once more if the index then holds more than is stored: vectors of
    /// artifacts no longer stored, which as many missing artifacts hid
    /// from the first count.
    async fn compared_and_worked(
        &self,
        idx: &dyn ArtifactIndexer,
        lock: &BackfillLock,
        pace: &IndexPace,
        progress: impl Fn(usize, usize),
    ) -> Result<Backfilled> {
        let noted = self.compare(idx, lock).await?;
        let mut done = self.worked(idx, noted, pace, &progress).await?;
        // A compare that noted nothing has listed every stray already (the
        // count was above what is stored as it ran): another would find
        // nothing more, at the cost of a walk.
        if noted > 0 && done.failed == 0 && idx.count().await? > self.stored_count().await? {
            let hidden = self.compare(idx, lock).await?;
            let after = self.worked(idx, hidden, pace, &progress).await?;
            done.total += after.total;
            done.indexed += after.indexed;
            done.removed += after.removed;
            done.failed = after.failed;
            done.failures = after.failures;
        }
        Ok(done)
    }

    /// Work the pending list once, after a compare found `noted`
    /// artifacts to differ, and say what came of it.
    async fn worked(
        &self,
        idx: &dyn ArtifactIndexer,
        noted: usize,
        pace: &IndexPace,
        progress: impl Fn(usize, usize),
    ) -> Result<Backfilled> {
        self.index.update(|state| {
            state.backfill = (noted > 0).then_some(BackfillProgress {
                indexed: 0,
                total: noted,
            })
        });
        let worked = self.work_due(idx, pace).await?;
        progress(worked.taken(), noted);
        let failing = self.failing().await?;
        let left = self.stored_and_pending().await?.1;
        let failures = failing.iter().take(FAILURES_KEPT).map(|one| IndexFailure {
            id: one.id.clone(),
            error: one.error.clone(),
        });
        let done = Backfilled {
            indexed: worked.indexed,
            total: noted,
            removed: worked.removed,
            failed: left,
            failures: failures.collect(),
        };
        self.index.update(|state| {
            state.pending = left;
            state.failing = failing;
        });
        Ok(done)
    }

    /// Fill an index that no hub serves, from Postgres: what
    /// `workforce artifacts reindex` does. Every stored artifact the index
    /// lacks or holds another text of is written to it, and what it holds
    /// of artifacts no longer stored removed, reporting `(done, total)`.
    ///
    /// It asks nothing of the pending list and writes nothing to it: that
    /// list is the serving index's, and a hub's keeper works it. Each
    /// write is the one the keeper makes (`settle`): what is
    /// stored is read again after it, and written again if it changed
    /// meanwhile.
    ///
    /// It stops after three artifacts in a row have failed, and says
    /// which could not be written; run again, it goes on with what still
    /// differs. Only a run that wrote everything marks the index
    /// compared.
    ///
    /// Refused ([`SeedRefused`]) while a hub serves an index of this
    /// kind, as a courtesy to the operator and no more: it goes by a
    /// Postgres session lock, which behind PgBouncer in transaction mode
    /// does nothing. What keeps a seeding safe beside a hub is the
    /// re-reading write.
    pub async fn seed_index(&self, progress: impl Fn(usize, usize)) -> Result<Backfilled> {
        let idx = self.indexer.as_deref().ok_or_else(|| {
            eyre::eyre!("no artifact index is configured: artifacts are found by keyword only")
        })?;
        let alone = deadpool_postgres::Object::take(self.pool.get().await?);
        let key = serving_lock(idx.backend());
        let unserved: bool = alone
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .await?
            .get(0);
        if !unserved {
            return Err(eyre::Report::new(SeedRefused(idx.backend())));
        }
        let done = self.fill_unserved(idx, progress).await;
        let _ = alone
            .execute("SELECT pg_advisory_unlock($1)", &[&key])
            .await;
        done
    }

    async fn fill_unserved(
        &self,
        idx: &dyn ArtifactIndexer,
        progress: impl Fn(usize, usize),
    ) -> Result<Backfilled> {
        idx.mark_incomplete().await?;
        let mut differing = self.differing(idx).await?;
        // What the index holds beyond that: of artifacts no longer stored.
        let stored = self.stored_count().await?;
        if idx.count().await? + differing.len() > stored {
            differing.extend(self.strays(idx).await?);
        }
        let mut done = Backfilled {
            total: differing.len(),
            ..Backfilled::default()
        };
        let mut in_a_row = 0;
        for (nth, id) in differing.iter().enumerate() {
            if in_a_row == 3 {
                done.failed += differing.len() - nth;
                break;
            }
            match self.settle(idx, id).await {
                Ok(hash) => {
                    in_a_row = 0;
                    match hash == DELETED {
                        true => done.removed += 1,
                        false => done.indexed += 1,
                    }
                }
                Err(e) => {
                    in_a_row += 1;
                    done.failed += 1;
                    if done.failures.len() < FAILURES_KEPT {
                        done.failures.push(IndexFailure {
                            id: id.clone(),
                            error: format!("{e:#}"),
                        });
                    }
                }
            }
            progress(nth + 1, differing.len());
        }
        if done.failed == 0 {
            idx.mark_complete().await?;
        }
        Ok(done)
    }

    /// Have the keeper try now what it is waiting to try again: for an
    /// operator who has put right whatever the index was failing on.
    /// Returns how many artifacts that is.
    pub async fn retry_now(&self) -> Result<u64> {
        let conn = self.pool.get().await?;
        let table = self.slot.pending_table;
        let waiting =
            format!("UPDATE {table} SET next_try_at = NULL WHERE next_try_at IS NOT NULL");
        let made_due = conn.execute(&waiting, &[]).await?;
        self.index.wake.notify_one();
        Ok(made_due)
    }

    /// Compare the index with Postgres, artifact by artifact, and note as
    /// pending whatever differs: each stored artifact the index lacks or
    /// holds another text of, and each vector whose artifact is no longer
    /// stored. Nothing is written to the index here: what is noted is
    /// indexed by whoever works the pending list, as every other artifact
    /// is. Returns how many differed.
    ///
    /// An index not yet marked is marked by this: compared with Postgres
    /// since it was made, so that it and the pending list between them
    /// account for every artifact. It is marked for the making that was
    /// there when the compare began ([`ArtifactIndexer::mark_complete`]
    /// refuses another), and not on the word of a lock since lost.
    async fn compare(&self, idx: &dyn ArtifactIndexer, lock: &BackfillLock) -> Result<usize> {
        let marked = idx.complete().await?;
        if !marked {
            idx.mark_incomplete().await?;
            self.index.update(|state| state.complete = Some(false));
        }
        let mut differing = self.differing(idx).await?;
        // Vectors of artifacts no longer stored show as more held than
        // stored. As many missing as there are of those hide them for
        // now: the count is out again once the missing are indexed, and
        // the compare that follows finds them.
        if idx.count().await? > self.stored_count().await? {
            differing.extend(self.strays(idx).await?);
        }
        lock.held().await?;
        let table = self.slot.pending_table;
        // Each with the hash of what is stored now; none for one removed.
        let note = format!(
            "INSERT INTO {table} (artifact_id, text_hash) \
             SELECT noted.id, coalesce((SELECT {TEXT_HASH} FROM artifacts a WHERE a.id = noted.id), $2) \
             FROM unnest($1::text[]) AS noted(id) \
             ON CONFLICT (artifact_id) DO NOTHING"
        );
        let conn = self.pool.get().await?;
        conn.execute(&note, &[&differing, &DELETED]).await?;
        drop(conn);
        if !marked {
            lock.held().await?;
            idx.mark_complete().await?;
        }
        Ok(differing.len())
    }

    /// The index's count beside Postgres's, read now; whether it is in
    /// step is kept for whoever asks how a search would go.
    async fn read_count(&self, idx: &dyn ArtifactIndexer) -> Result<Reading> {
        let (stored, pending) = self.stored_and_pending().await?;
        let read = Reading {
            held: idx.count().await?,
            stored,
            pending,
        };
        self.index
            .update(|state| state.in_step = Some(read.in_step()));
        Ok(read)
    }

    /// How many artifacts are stored, and how many are noted as pending:
    /// both now, in one reading.
    async fn stored_and_pending(&self) -> Result<(usize, usize)> {
        let table = self.slot.pending_table;
        let both =
            format!("SELECT (SELECT count(*) FROM artifacts), (SELECT count(*) FROM {table})");
        let conn = self.pool.get().await?;
        let row = conn.query_one(&both, &[]).await?;
        let (stored, pending): (i64, i64) = (row.get(0), row.get(1));
        Ok((usize::try_from(stored)?, usize::try_from(pending)?))
    }

    /// How many artifacts are stored.
    async fn stored_count(&self) -> Result<usize> {
        let conn = self.pool.get().await?;
        let stored: i64 = conn
            .query_one("SELECT count(*) FROM artifacts", &[])
            .await?
            .get(0);
        Ok(usize::try_from(stored)?)
    }

    /// One round of keeping the index in line with Postgres, and what the
    /// keeper does after it.
    ///
    /// The pending artifacts that are due are worked ([`Self::work_due`]).
    /// Then the index is looked at, whatever that work came to
    /// ([`Self::looked_at`]): compared with Postgres when it is to be, and
    /// what the compare notes worked at once.
    ///
    /// What the index does to an artifact is never an error of the round:
    /// the artifact stays pending. Why the round left something undone is
    /// said afresh each round, for whoever asks how the index is. A round
    /// in which the index took nothing, or could not be looked at, is
    /// followed by the keeper's fixed wait; one that took something, with
    /// more due, by another at once; and otherwise by a wait until the
    /// next artifact is due, or the keeper's watch.
    async fn round(&self, idx: &dyn ArtifactIndexer, pace: &IndexPace) -> Result<Next> {
        let first = self.work_due(idx, pace).await?;
        let mut taken = first.taken();
        let mut none_taken = first.tried > 0 && first.taken() == 0;
        let mut said = Vec::new();
        // Said first, at the end, unless the compare then had the index
        // take something.
        let mut took_none = None;
        if none_taken {
            // What the index said of the last artifact it was given.
            let failing = self.failing().await?;
            let last = failing.iter().max_by_key(|one| one.next_try_at);
            let error = last.map_or_else(String::new, |one| format!(": {}", one.error));
            took_none = Some(format!(
                "the artifact index took none of the {} artifact(s) it was given{error}",
                first.tried
            ));
        }
        // Looked at even when the work took nothing: artifacts that always
        // fail are due in every round, and an index lost behind them would
        // otherwise never be seen to be.
        match self.looked_at(idx, pace, &mut said).await {
            Ok(compared) => {
                let filled = compared.map_or(0, |done| done.indexed + done.removed);
                // The compare had the index take something: the round did
                // something, whatever its first work came to, and is not
                // said, or waited after, as one that took none.
                if filled > 0 && took_none.take().is_some() {
                    none_taken = false;
                }
                taken += filled;
            }
            // Another process is comparing: what it notes is worked here
            // as it appears. Not looked at again until the fixed wait is
            // out, though a compare is due: the lock is not asked for as
            // fast as the keeper can go round, for as long as that lasts.
            Err(e) if e.downcast_ref::<BackfillBusy>().is_some() => {
                said.push(format!(
                    "{e}: compared with Postgres there; looked at again after {:?}",
                    pace.retry
                ));
                none_taken = true;
            }
            Err(e) => {
                tracing::warn!(error = ?e, "the artifact index could not be looked at or compared with Postgres; it is looked at again next round");
                said.push(format!("{e:#}"));
                none_taken = true;
            }
        }
        said.splice(0..0, took_none);
        self.index.update(|state| {
            state.last_error = (!said.is_empty()).then(|| said.join("; "));
        });
        let (due, earliest) = self.note_pending().await?;
        // Given artifacts by a compare, and took none of them.
        none_taken |= taken == 0 && due > 0;
        let now = (self.clock)();
        Ok(if none_taken {
            Next::After(pace.retry)
        } else if taken > 0 && due > 0 {
            Next::AtOnce
        } else {
            let watch = now + pace.watch;
            Next::At(earliest.map_or(watch, |at| at.min(watch)))
        })
    }

    /// Record how the pending list stands, for whoever asks how the index
    /// is; and say how many artifacts are due now and when the first of
    /// the others is.
    async fn note_pending(&self) -> Result<(usize, Option<DateTime<Utc>>)> {
        let table = self.slot.pending_table;
        let now = (self.clock)();
        let read = format!(
            "SELECT count(*), \
                    count(*) FILTER (WHERE next_try_at IS NULL OR next_try_at <= $1), \
                    min(next_try_at) FILTER (WHERE next_try_at > $1), \
                    (SELECT count(*) FROM artifacts) FROM {table}"
        );
        let conn = self.pool.get().await?;
        let row = conn.query_one(&read, &[&now]).await?;
        drop(conn);
        let (pending, due, stored): (i64, i64, i64) = (row.get(0), row.get(1), row.get(3));
        let earliest: Option<DateTime<Utc>> = row.get(2);
        let (pending, stored) = (usize::try_from(pending)?, usize::try_from(stored)?);
        let failing = self.failing().await?;
        let pace = self.index.pace();
        let at_cap = |tries: i32| {
            let doublings = u32::try_from(tries.max(1) - 1).unwrap_or(0).min(30);
            pace.backoff.saturating_mul(2u32.saturating_pow(doublings)) >= pace.backoff_cap
        };
        self.index.update(|state| {
            state.pending = pending;
            state.failing_at_cap = failing.iter().filter(|one| at_cap(one.tries)).count();
            state.oldest_failure_at = failing.iter().filter_map(|one| one.first_failed_at).min();
            state.failing = failing;
            // As a search decides: marked, within the tolerance, and the
            // count in step at the last reading.
            let by_index = state.complete == Some(true)
                && pending <= search_tolerance(stored)
                && state.in_step != Some(false);
            state.search = Some(if by_index { "index" } else { "keyword" });
            state.counted_at = Some(now);
        });
        Ok((usize::try_from(due)?, earliest))
    }

    /// Look at the index: whether it is marked compared with Postgres
    /// since it was made, and, if it is, how its count stands beside
    /// Postgres's ([`in_step`]). Then compare the two artifact by artifact
    /// if they are to be, and work what that notes. `None` when they were
    /// not compared.
    ///
    /// They are compared at the first look of this process and every
    /// `pace.reconcile` after, for an index that keeps the hash of each
    /// text; and when the index is not marked, or its count is out of
    /// step at two readings running. After a compare that ended badly
    /// ([`Compared::ended_well`]) the last two wait out `pace.backoff_cap`
    /// from it, unless a reading finds the count in step meanwhile: a
    /// compare that could not put the index right is not run again round
    /// after round, at the cost of a walk of every artifact each time.
    /// Why one waits is pushed to `said`, as is two readings that
    /// disagreed.
    async fn looked_at(
        &self,
        idx: &dyn ArtifactIndexer,
        pace: &IndexPace,
        said: &mut Vec<String>,
    ) -> Result<Option<Backfilled>> {
        let marked = idx.complete().await?;
        self.index.update(|state| state.complete = Some(marked));
        // Read twice before it is believed: Postgres and the index are
        // read a moment apart, and one write between the two reads puts
        // them out by one with nothing wrong. A compare walks every
        // artifact, and is not started on one reading.
        let mut readings = Vec::with_capacity(2);
        if marked {
            let first = self.read_count(idx).await?;
            readings.push(first);
            if !first.in_step() {
                readings.push(self.read_count(idx).await?);
            }
        }
        let miscounted = readings.len() == 2 && readings.iter().all(|read| !read.in_step());
        if let [first, second] = readings[..] {
            if second.in_step() {
                said.push(format!(
                    "two readings of the artifact index's count disagreed: it held {} of {} stored ({} pending), then {}",
                    first.held, first.stored, first.pending, second.held
                ));
            }
        }
        let now = (self.clock)();
        let last = {
            let mut last = self
                .index
                .compared_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // The count read in step: whatever a compare left wrong has
            // come right since.
            if readings.iter().any(Reading::in_step) {
                if let Some(last) = last.as_mut() {
                    last.ended_well = true;
                }
            }
            *last
        };
        let reconcile =
            idx.compares_text() && last.is_none_or(|last| since(now, last.at) >= pace.reconcile);
        let asked = !marked || miscounted;
        let held_off =
            last.filter(|last| !last.ended_well && since(now, last.at) < pace.backoff_cap);
        let compare = reconcile || (asked && held_off.is_none());
        if !compare {
            if let (true, Some(last)) = (asked, held_off) {
                let again = last.at + pace.backoff_cap;
                said.push(match readings.last() {
                    Some(read) if miscounted => format!(
                        "the artifact index holds {} artifacts, out of step with Postgres ({} stored, {} pending), after the compare at {}; compared again after {again}",
                        read.held, read.stored, read.pending, last.at
                    ),
                    _ => format!(
                        "the artifact index is not marked compared, and the compare at {} did not end well; compared again after {again}",
                        last.at
                    ),
                });
            }
            return Ok(None);
        }
        if miscounted {
            tracing::warn!(
                "the artifact index holds a number of artifacts it could not hold in step with Postgres; comparing them"
            );
        }
        let one_at_a_time =
            BackfillLock::acquire(&self.pool, self.slot.compare_lock, false).await?;
        let done = self
            .compared_and_worked(idx, &one_at_a_time, pace, |_, _| {})
            .await;
        one_at_a_time.release().await;
        let done = match done {
            Ok(done) => done,
            Err(e) => {
                self.ended((self.clock)(), false);
                return Err(e);
            }
        };
        // Ended well only once the count is read in step after it.
        self.compared(&done, false);
        let after = self.read_count(idx).await?;
        if after.in_step() {
            self.ended((self.clock)(), true);
        } else {
            said.push(format!(
                "the artifact index holds {} artifacts, out of step with Postgres ({} stored, {} pending), after a compare that could not explain it; compared again after {}",
                after.held,
                after.stored,
                after.pending,
                (self.clock)() + pace.backoff_cap
            ));
        }
        if done.total > 0 {
            tracing::info!(
                differed = done.total,
                indexed = done.indexed,
                removed = done.removed,
                pending = done.failed,
                "artifact index compared with Postgres"
            );
        }
        Ok(Some(done))
    }

    /// Record that a compare ended just now, at `at`, and how.
    fn ended(&self, at: DateTime<Utc>, well: bool) {
        let mut last = self
            .index
            .compared_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *last = Some(Compared {
            at,
            ended_well: well,
        });
    }

    /// Record that the index was compared with Postgres just now, and
    /// what came of it; `well` as [`Compared::ended_well`].
    fn compared(&self, done: &Backfilled, well: bool) {
        let now = (self.clock)();
        self.ended(now, well);
        self.index.update(|state| {
            state.backfill = None;
            state.complete = Some(true);
            state.reconciled_at = Some(now);
            if done.total > 0 || state.last_backfill.is_none() {
                state.last_backfill = Some(done.clone());
            }
        });
    }

    /// A round as the tests before the keeper's redesign asked for one:
    /// whether nothing is left pending after it.
    #[cfg(test)]
    async fn index_round(&self, idx: &dyn ArtifactIndexer) -> Result<bool> {
        let pace = self.index.pace();
        self.round(idx, &pace).await?;
        Ok(self.stored_and_pending().await?.1 == 0)
    }

    /// Keep the index complete in the background, for as long as the store
    /// lives.
    ///
    /// A round runs now, then every `pace.watch`, or sooner when a write is
    /// noted or a search finds the index wanting. It puts right what is
    /// pending and checks that the index is recorded complete and holds as
    /// many artifacts as are stored (each round, whatever its work came
    /// to). The first round, and one every `pace.reconcile` after, also
    /// compares the two artifact by artifact, by id and text hash, which
    /// finds what nothing noted: artifacts written while the hub indexed
    /// elsewhere, and an index restored to an earlier state. So without
    /// anyone acting the index is right within `pace.reconcile`, plus the
    /// time of a repair, of falling behind in any way, and at once after a
    /// hub starts; within `pace.backoff_cap` more after a compare that
    /// could not put it right ([`Self::looked_at`]).
    ///
    /// After a round in which the index took none of what it was given the
    /// next comes after `pace.retry`, a fixed wait; after one that took
    /// something with more due, at once; otherwise when the next artifact
    /// is due, at most `pace.watch` (see [`Next`]).
    /// While the index is not complete, search goes by keyword;
    /// [`Self::index_state`] says how it stands.
    pub fn keep_index_complete(self: &Arc<Self>, pace: IndexPace) {
        if self.indexer.is_none() {
            return;
        }
        *self.index.pace.lock().unwrap_or_else(|e| e.into_inner()) = pace;
        // A try may take `pace.deadline`; past that and three watches more,
        // a keeper that has not moved on is stuck.
        self.index
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stall = pace.deadline + pace.watch * LEASE_WATCHES;
        self.index.progressed();
        let store = Arc::downgrade(self);
        let keeping = Arc::clone(&self.index);
        let alive = KeeperAlive::new(Arc::clone(&self.index));
        tokio::spawn(async move {
            let _alive = alive;
            let mut serving: Option<deadpool_postgres::ClientWrapper> = None;
            // Chooses a line of the log and nothing else: whether the last
            // round's outcome was said already, and when.
            let mut said: Option<(bool, std::time::Instant)> = None;
            loop {
                let Some(store) = store.upgrade() else {
                    return;
                };
                let Some(idx) = store.indexer.clone() else {
                    return;
                };
                keeping.progressed();
                store.say_serving(idx.backend(), &mut serving).await;
                // Whether to compare is the round's to decide, on the
                // store's clock ([`KnowledgeStore::looked_at`]).
                let round = store.round(idx.as_ref(), &pace).await;
                let now = (store.clock)();
                let compares = idx.compares_text();
                drop(store);
                let next = match round {
                    Ok(next) => next,
                    // Postgres did not answer, or the like: the round
                    // itself could not be run.
                    Err(e) => {
                        tracing::warn!(error = ?e, retry_in = ?pace.retry, "a round of keeping the artifact index could not be run");
                        keeping.update(|state| state.last_error = Some(format!("{e:#}")));
                        Next::After(pace.retry)
                    }
                };
                let took_none = matches!(next, Next::After(_));
                let lately = std::time::Duration::from_secs(600);
                if said.is_none_or(|(as_then, at)| {
                    as_then != took_none || (took_none && at.elapsed() >= lately)
                }) {
                    match took_none {
                        true => {
                            tracing::warn!(retry_in = ?pace.retry, "a round of keeping the artifact index did nothing: the index took none of what it was given, could not be looked at, or another process is comparing it (health says which); each artifact is tried again when it is due")
                        }
                        false if said.is_some() => {
                            tracing::info!("the artifact index is taking artifacts again")
                        }
                        false => {}
                    }
                    said = Some((took_none, std::time::Instant::now()));
                }
                match next {
                    Next::AtOnce => {}
                    // The index took nothing: not asked again sooner,
                    // whatever is written meanwhile.
                    Next::After(wait) => {
                        keeping.waiting(wait);
                        tokio::time::sleep(wait).await
                    }
                    Next::At(then) => {
                        // Not past when the index may next be compared.
                        let compared = *keeping
                            .compared_at
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let wait = keeper_wait(then, now, compared, &pace, compares);
                        keeping.waiting(wait);
                        tokio::select! {
                            () = tokio::time::sleep(wait) => {}
                            () = keeping.wake.notified() => {}
                        }
                    }
                }
            }
        });
    }

    /// Hold, shared, the session lock that says a hub serves an index of
    /// this kind, on a connection kept for it; taken again when that
    /// connection is lost. A courtesy to whoever runs a seeding against
    /// this index by mistake ([`Self::seed_index`]): nothing else goes by it.
    async fn say_serving(
        &self,
        backend: &'static str,
        held_on: &mut Option<deadpool_postgres::ClientWrapper>,
    ) {
        if let Some(conn) = held_on {
            if conn.query_one("SELECT 1", &[]).await.is_ok() {
                return;
            }
            *held_on = None;
        }
        let Ok(conn) = self.pool.get().await else {
            return;
        };
        let conn = deadpool_postgres::Object::take(conn);
        let key = serving_lock(backend);
        let shared = conn
            .query_one("SELECT pg_try_advisory_lock_shared($1)", &[&key])
            .await;
        match shared.map(|row| row.get::<_, bool>(0)) {
            Ok(true) => *held_on = Some(conn),
            Ok(false) => tracing::warn!(
                backend,
                "a seeding run (`workforce artifacts reindex`) holds this hub's artifact index; it is for an index no hub serves"
            ),
            Err(_) => {}
        }
    }

    /// What the index finds for `query`, if the index can be relied on
    /// and answers within the store's search deadline, where it has one.
    /// Otherwise why not, for the search to go by keyword: at once when
    /// the index fails; after the deadline when it accepts the question
    /// and does not answer; and at once again, the index left alone, once
    /// several searches in a row have waited the deadline out (see
    /// [`workforce_common::breaker`]).
    async fn ranked_by_index(
        &self,
        idx: &dyn ArtifactIndexer,
        query: &str,
        limit: usize,
    ) -> std::result::Result<(Vec<(String, f32)>, usize), Skipped> {
        // An index marked compared may still be filling, or behind: what
        // is pending it does not hold as stored and could not find. A few
        // are borne; more, and the search goes by keyword. Asked of
        // Postgres, and outside the index's deadline.
        let (stored, pending) = match self.stored_and_pending().await {
            Ok((stored, pending)) if pending > search_tolerance(stored) => {
                return Err(Skipped::Behind { pending, stored });
            }
            Ok(both) => both,
            Err(e) => return Err(Skipped::Failed(format!("{e:#}"))),
        };
        // The index's own time, its embedding of the question in it: what
        // a read shadow compares with the mirror's.
        let began = std::time::Instant::now();
        let asked = match &self.search {
            None => ask_index(idx, query, limit).await,
            Some(wait) => {
                let Some(ticket) = wait.breaker.admit() else {
                    return Err(Skipped::Resting);
                };
                let asked = tokio::time::timeout(wait.deadline, ask_index(idx, query, limit));
                match asked.await {
                    Ok(asked) => {
                        wait.breaker.succeeded(ticket);
                        asked
                    }
                    Err(_) => {
                        let opened = wait.breaker.failed(ticket);
                        tracing::warn!(deadline = ?wait.deadline, left_alone = opened, rest = ?wait.breaker.rest(), "the artifact index did not answer in time; searching by keyword");
                        return Err(Skipped::TimedOut(wait.deadline));
                    }
                }
            }
        };
        match asked {
            Ok(Asked::Ranked { ranked, held }) if in_step(held, stored, pending) => {
                self.observe_read(query, limit, &ranked, began.elapsed());
                Ok((ranked, pending))
            }
            // The keeper is woken to look for itself: Postgres and the
            // index were read a moment apart, and a write between the two
            // reads can put them out by one with nothing wrong.
            Ok(Asked::Ranked { held, .. }) => {
                tracing::warn!(
                    held,
                    stored,
                    pending,
                    "the artifact index is out of step with Postgres; searching by keyword"
                );
                self.index.wake.notify_one();
                Err(Skipped::OutOfStep {
                    held,
                    stored,
                    pending,
                })
            }
            Ok(Asked::Incomplete) => Err(Skipped::Incomplete),
            Ok(Asked::Refused(why)) => Err(Skipped::NotOurs(why)),
            // Failed at once: the next search loses nothing by asking.
            Err(e) => {
                tracing::warn!(error = ?e, "the artifact index did not answer; searching by keyword");
                Err(Skipped::Failed(format!("{e:#}")))
            }
        }
    }

    /// Index an artifact just written or deleted, in the background, if an
    /// indexer is wired: tag it, when `tag`, writing back any tags that
    /// adds, then [`Self::settle_noted`]. Its write is already noted as
    /// pending, by Postgres, so whatever becomes of this task the index is
    /// put right later.
    fn spawn_settle(&self, artifact: Option<Artifact>, id: String) {
        self.wake_mirror();
        let Some(idx) = self.indexer.clone() else {
            return;
        };
        let mirror_wake = self.mirror_wake.get().cloned();
        let store = Self {
            pool: self.pool.clone(),
            indexer: None,
            index: Arc::clone(&self.index),
            search: self.search.clone(),
            clock: Arc::clone(&self.clock),
            slot: self.slot,
            mirror_wake: std::sync::OnceLock::new(),
            read_shadow: std::sync::OnceLock::new(),
        };
        tokio::spawn(async move {
            let settled = async {
                if let Some(artifact) = artifact {
                    let tags = idx.tags(&artifact).await;
                    if tags != artifact.tags {
                        let conn = store.pool.get().await?;
                        conn.execute(
                            "UPDATE artifacts SET tags = $2 WHERE id = $1",
                            &[&id, &serde_json::to_string(&tags)?],
                        )
                        .await?;
                        // Noted afresh, for the mirror too.
                        if let Some(mirror) = &mirror_wake {
                            mirror.wake.notify_one();
                        }
                    }
                }
                let pace = store.index.pace();
                store.try_one(idx.as_ref(), &id, &pace).await
            };
            // Not taken: noted, and the keeper woken, which may have
            // nothing else due for as long as its watch.
            match settled.await {
                Ok(Tried::Failed) => store.index.wake.notify_one(),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(id, error = ?e, "artifact not indexed; it stays noted, to index later");
                    store.index.wake.notify_one();
                }
            }
        });
    }

    /// Load an artifact ignoring ACL (internal owner/mutation checks).
    async fn load_raw(&self, id: &str) -> Result<Option<Artifact>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!("SELECT {SELECT_COLS} FROM artifacts WHERE id = $1"),
                &[&id],
            )
            .await?;
        row.as_ref().map(row_to_artifact).transpose()
    }

    /// Whether `caller` may read `artifact` (consults the shares table).
    async fn caller_can_read(&self, artifact: &Artifact, caller: &Caller) -> Result<bool> {
        if open_to(artifact, caller) {
            return Ok(true);
        }
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT 1 FROM artifact_shares WHERE artifact_id = $1 AND user_id = $2",
                &[&artifact.id, &caller.key],
            )
            .await?;
        Ok(row.is_some())
    }

    /// SQL fragment + bound params implementing read ACL for list/search.
    /// Returns the WHERE-clause body and the params it consumed (`$1`, `$2`).
    fn acl_clause(caller: &Caller, params: &mut Vec<Box<dyn ToSql + Sync + Send>>) -> String {
        if caller.is_admin {
            return "TRUE".to_string();
        }
        params.push(Box::new(caller.key.clone()));
        let k = params.len();
        format!(
            "(visibility IN ('public', 'team') OR owner_id = ${k} \
             OR id IN (SELECT artifact_id FROM artifact_shares WHERE user_id = ${k}))"
        )
    }

    /// Insert a row, skipping on id conflict. Returns rows affected (0 = the
    /// id already existed). Shared by [`Self::seed`] and `create`.
    async fn insert_row(&self, artifact: &Artifact) -> Result<u64> {
        let conn = self.pool.get().await?;
        let tags = serde_json::to_string(&artifact.tags)?;
        Ok(conn
            .execute(
                "INSERT INTO artifacts \
                 (id, artifact_type, title, content, owner_id, visibility, tags, status, \
                  version, created_by, last_modified_by, created_at, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &artifact.id,
                    &artifact.artifact_type.to_string(),
                    &artifact.title,
                    &artifact.content,
                    &artifact.owner_id,
                    &artifact.visibility.to_string(),
                    &tags,
                    &artifact.status.to_string(),
                    &(artifact.version as i32),
                    &artifact.created_by,
                    &artifact.last_modified_by,
                    &artifact.created_at,
                    &artifact.updated_at,
                ],
            )
            .await?)
    }

    /// Insert an artifact only if absent — idempotent disk→DB migration
    /// (DW-548). Returns `true` if inserted (and indexes it); never errors on
    /// a pre-existing id.
    pub async fn seed(&self, artifact: &Artifact) -> Result<bool> {
        let inserted = self.insert_row(artifact).await? > 0;
        if inserted {
            self.spawn_settle(Some(artifact.clone()), artifact.id.clone());
        }
        Ok(inserted)
    }
}

#[async_trait]
impl ArtifactBackend for KnowledgeStore {
    async fn create(&self, artifact: &Artifact) -> Result<()> {
        if self.insert_row(artifact).await? == 0 {
            return Err(eyre::eyre!("Artifact '{}' already exists", artifact.id));
        }
        self.spawn_settle(Some(artifact.clone()), artifact.id.clone());
        Ok(())
    }

    async fn read(&self, id: &str, caller: &Caller) -> Result<Option<Artifact>> {
        let Some(artifact) = self.load_raw(id).await? else {
            return Ok(None);
        };
        if self.caller_can_read(&artifact, caller).await? {
            Ok(Some(artifact))
        } else {
            Ok(None)
        }
    }

    async fn update(
        &self,
        id: &str,
        content: &str,
        modified_by: &str,
        caller: &Caller,
    ) -> Result<Artifact> {
        let mut artifact = self
            .load_raw(id)
            .await?
            .ok_or_else(|| eyre::eyre!("Artifact '{id}' not found"))?;
        require_owner(&artifact, caller)?;
        if artifact.status == ArtifactStatus::Finalized {
            return Err(eyre::eyre!(
                "Artifact '{id}' is finalized and cannot be modified"
            ));
        }
        artifact.content = content.to_string();
        artifact.last_modified_by = modified_by.to_string();
        artifact.version += 1;
        artifact.updated_at = Utc::now();

        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE artifacts SET content = $2, last_modified_by = $3, version = $4, \
             updated_at = $5 WHERE id = $1",
            &[
                &artifact.id,
                &artifact.content,
                &artifact.last_modified_by,
                &(artifact.version as i32),
                &artifact.updated_at,
            ],
        )
        .await?;
        self.spawn_settle(Some(artifact.clone()), artifact.id.clone());
        Ok(artifact)
    }

    async fn finalize(&self, id: &str, caller: &Caller) -> Result<Artifact> {
        let mut artifact = self
            .load_raw(id)
            .await?
            .ok_or_else(|| eyre::eyre!("Artifact '{id}' not found"))?;
        require_owner(&artifact, caller)?;
        artifact.status = ArtifactStatus::Finalized;
        artifact.updated_at = Utc::now();

        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE artifacts SET status = $2, updated_at = $3 WHERE id = $1",
            &[
                &artifact.id,
                &artifact.status.to_string(),
                &artifact.updated_at,
            ],
        )
        .await?;
        Ok(artifact)
    }

    async fn delete(&self, id: &str, caller: &Caller) -> Result<()> {
        let artifact = self
            .load_raw(id)
            .await?
            .ok_or_else(|| eyre::eyre!("Artifact '{id}' not found"))?;
        require_owner(&artifact, caller)?;
        let conn = self.pool.get().await?;
        conn.execute("DELETE FROM artifacts WHERE id = $1", &[&id])
            .await?;
        self.spawn_settle(None, id.to_string());
        Ok(())
    }

    async fn list(&self, caller: &Caller, filter: &ArtifactFilter) -> Result<Vec<Artifact>> {
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        let mut sql = format!("SELECT {SELECT_COLS} FROM artifacts WHERE ");
        sql.push_str(&Self::acl_clause(caller, &mut params));
        push_filter(&mut sql, &mut params, filter);
        sql.push_str(" ORDER BY updated_at DESC");

        let conn = self.pool.get().await?;
        let param_refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let rows = conn.query(&sql, &param_refs).await?;
        rows.iter().map(row_to_artifact).collect()
    }

    async fn set_visibility(
        &self,
        id: &str,
        visibility: Visibility,
        caller: &Caller,
    ) -> Result<()> {
        let artifact = self
            .load_raw(id)
            .await?
            .ok_or_else(|| eyre::eyre!("Artifact '{id}' not found"))?;
        require_owner(&artifact, caller)?;
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE artifacts SET visibility = $2, updated_at = $3 WHERE id = $1",
            &[&id, &visibility.to_string(), &Utc::now()],
        )
        .await?;
        Ok(())
    }

    async fn share_with(&self, id: &str, user_id: &str, caller: &Caller) -> Result<()> {
        let artifact = self
            .load_raw(id)
            .await?
            .ok_or_else(|| eyre::eyre!("Artifact '{id}' not found"))?;
        require_owner(&artifact, caller)?;
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO artifact_shares (artifact_id, user_id, granted_at) \
             VALUES ($1, $2, $3) ON CONFLICT (artifact_id, user_id) DO NOTHING",
            &[&id, &user_id, &Utc::now()],
        )
        .await?;
        Ok(())
    }

    async fn set_owner(&self, id: &str, owner: &str) -> Result<()> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "UPDATE artifacts SET owner_id = $2, updated_at = $3 WHERE id = $1",
            &[&id, &owner, &Utc::now()],
        )
        .await?;
        tx.execute(
            "DELETE FROM artifact_shares WHERE artifact_id = $1 AND user_id = $2",
            &[&id, &owner],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn shares_of(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT user_id FROM artifact_shares WHERE artifact_id = $1 ORDER BY user_id",
                &[&id],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn grants_of(&self, user: &str) -> Result<Vec<String>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT artifact_id FROM artifact_shares WHERE user_id = $1 ORDER BY artifact_id",
                &[&user],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn revoke_share(&self, id: &str, user: &str) -> Result<bool> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute(
                "DELETE FROM artifact_shares WHERE artifact_id = $1 AND user_id = $2",
                &[&id, &user],
            )
            .await?
            > 0)
    }

    async fn search(
        &self,
        query: &str,
        caller: &Caller,
        filter: &ArtifactFilter,
        limit: usize,
    ) -> Result<Vec<ScoredArtifact>> {
        Ok(self.search_told(query, caller, filter, limit).await?.hits)
    }

    async fn search_told(
        &self,
        query: &str,
        caller: &Caller,
        filter: &ArtifactFilter,
        limit: usize,
    ) -> Result<Searched> {
        // Semantic path, once the index holds every artifact: ask the
        // indexer for ranked ids, then re-load and ACL-filter here so
        // visibility is enforced in one place. Until then, keywords.
        let by_keyword = match &self.indexer {
            None => Skipped::NoIndex,
            Some(idx) => match self.ranked_by_index(idx.as_ref(), query, limit).await {
                Ok((ranked, pending)) => {
                    let mut hits = Vec::new();
                    for (id, score) in ranked {
                        if let Some(artifact) = self.read(&id, caller).await? {
                            if filter_matches(&artifact, filter) {
                                hits.push(ScoredArtifact { artifact, score });
                                if hits.len() >= limit {
                                    break;
                                }
                            }
                        }
                    }
                    return Ok(Searched {
                        hits,
                        by_keyword: None,
                        not_indexed: pending,
                    });
                }
                Err(why) => {
                    // Found wanting: unmarked, lost and made again, or not
                    // answering. The keeper is told, and what was known of
                    // the index is known no longer. Not by a search that
                    // never asked it, the index being left alone: that
                    // found out nothing.
                    if why != Skipped::Resting {
                        self.index
                            .update(|state| state.complete = state.complete.filter(|c| !c));
                        self.index.wake.notify_one();
                    }
                    why
                }
            },
        };

        // Fallback: substring match over title/content/tags.
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        let mut sql = format!("SELECT {SELECT_COLS} FROM artifacts WHERE ");
        sql.push_str(&Self::acl_clause(caller, &mut params));
        push_filter(&mut sql, &mut params, filter);
        params.push(Box::new(format!("%{}%", query.to_lowercase())));
        let q = params.len();
        sql.push_str(&format!(
            " AND (lower(title) LIKE ${q} OR lower(content) LIKE ${q} OR lower(tags) LIKE ${q})"
        ));
        sql.push_str(" ORDER BY updated_at DESC");

        let conn = self.pool.get().await?;
        let param_refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let rows = conn.query(&sql, &param_refs).await?;
        let mut out = Vec::new();
        for row in rows.iter() {
            out.push(ScoredArtifact {
                artifact: row_to_artifact(row)?,
                score: 0.0,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(Searched {
            hits: out,
            by_keyword: Some(by_keyword),
            not_indexed: 0,
        })
    }
}

/// Owner-or-admin gate for mutating operations.
fn require_owner(artifact: &Artifact, caller: &Caller) -> Result<()> {
    if caller.is_admin || artifact.owner_id == caller.key {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "permission denied: '{}' is owned by another user",
            artifact.id
        ))
    }
}

/// Whether `caller` may read `artifact` without a grant naming them.
fn open_to(artifact: &Artifact, caller: &Caller) -> bool {
    caller.is_admin || artifact.visibility != Visibility::Private || artifact.owner_id == caller.key
}

/// Append optional `artifact_type` / `visibility` / `tag` filters to a query.
fn push_filter(
    sql: &mut String,
    params: &mut Vec<Box<dyn ToSql + Sync + Send>>,
    filter: &ArtifactFilter,
) {
    if let Some(t) = &filter.artifact_type {
        params.push(Box::new(t.to_string()));
        sql.push_str(&format!(" AND artifact_type = ${}", params.len()));
    }
    if let Some(v) = &filter.visibility {
        params.push(Box::new(v.to_string()));
        sql.push_str(&format!(" AND visibility = ${}", params.len()));
    }
    if let Some(tag) = &filter.tag {
        // tags is a JSON array string, e.g. `["rust","db"]`.
        params.push(Box::new(format!("%\"{}\"%", tag.trim().to_lowercase())));
        sql.push_str(&format!(" AND tags LIKE ${}", params.len()));
    }
}

/// In-Rust equivalent of [`push_filter`], for post-filtering semantic hits.
fn filter_matches(a: &Artifact, filter: &ArtifactFilter) -> bool {
    if let Some(t) = &filter.artifact_type {
        if &a.artifact_type != t {
            return false;
        }
    }
    if let Some(v) = &filter.visibility {
        if &a.visibility != v {
            return false;
        }
    }
    if let Some(tag) = &filter.tag {
        let tag = tag.trim().to_lowercase();
        if !a.tags.iter().any(|t| t == &tag) {
            return false;
        }
    }
    true
}

/// Map a Postgres row to an [`Artifact`].
fn row_to_artifact(row: &tokio_postgres::Row) -> Result<Artifact> {
    let artifact_type: String = row.try_get("artifact_type")?;
    let visibility: String = row.try_get("visibility")?;
    let status: String = row.try_get("status")?;
    let tags_json: String = row.try_get("tags")?;
    let version: i32 = row.try_get("version")?;
    Ok(Artifact {
        id: row.try_get("id")?,
        artifact_type: artifact_type.parse().unwrap_or(ArtifactType::Notes),
        title: row.try_get("title")?,
        content: row.try_get("content")?,
        owner_id: row.try_get("owner_id")?,
        visibility: visibility.parse().unwrap_or(Visibility::Private),
        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        status: parse_status(&status),
        version: version.max(0) as u32,
        created_by: row.try_get("created_by")?,
        last_modified_by: row.try_get("last_modified_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn parse_status(s: &str) -> ArtifactStatus {
    match s {
        "finalized" => ArtifactStatus::Finalized,
        _ => ArtifactStatus::Draft,
    }
}

// ─────────────────────────── File fallback ───────────────────────────

/// Per-agent file backend used when Postgres is unavailable.
///
/// Stores each artifact, with the users it is shared with, as
/// `<base>/artifacts-v2/<id>.json`. Every user of the agent shares the
/// directory, so reads and mutations are gated exactly as in
/// [`KnowledgeStore`]; `search` degrades to a substring match.
pub struct FsArtifactBackend {
    dir: PathBuf,
}

/// Where a file backend keeps its artifacts, beneath its base directory.
const FS_ARTIFACT_DIR: &str = "artifacts-v2";

/// An artifact as a file holds it. Files written before sharing was kept
/// carry no `shares` and read back as shared with nobody.
#[derive(Serialize, Deserialize)]
struct StoredArtifact {
    #[serde(flatten)]
    artifact: Artifact,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    shares: BTreeSet<String>,
}

impl StoredArtifact {
    fn readable_by(&self, caller: &Caller) -> bool {
        open_to(&self.artifact, caller) || self.shares.contains(&caller.key)
    }
}

impl FsArtifactBackend {
    pub fn create_at(base_dir: &Path) -> Result<Self> {
        let dir = base_dir.join(FS_ARTIFACT_DIR);
        workforce_common::private_fs::ensure_private_dir(&dir)
            .wrap_err_with(|| format!("creating artifact dir: {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// The backend under `base_dir` when it already has one, without
    /// creating it.
    pub fn existing_at(base_dir: &Path) -> Option<Self> {
        let dir = base_dir.join(FS_ARTIFACT_DIR);
        dir.is_dir().then_some(Self { dir })
    }

    /// The file for `id`, which the model may supply: one that is not a
    /// single name would land outside the store.
    fn path(&self, id: &str) -> Result<PathBuf> {
        if !workforce_common::config_files::is_plain_name(id) {
            eyre::bail!("'{id}' is not an artifact id");
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    fn load(&self, id: &str) -> Result<Option<StoredArtifact>> {
        let path = self.path(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&raw)?))
    }

    fn load_existing(&self, id: &str) -> Result<StoredArtifact> {
        self.load(id)?
            .ok_or_else(|| eyre::eyre!("Artifact '{id}' not found"))
    }

    /// The artifact `id` for `caller` to change: it must exist and be theirs.
    fn load_owned(&self, id: &str, caller: &Caller) -> Result<StoredArtifact> {
        let stored = self.load_existing(id)?;
        require_owner(&stored.artifact, caller)?;
        Ok(stored)
    }

    fn write(&self, stored: &StoredArtifact) -> Result<()> {
        let raw = serde_json::to_string_pretty(stored)?;
        fs::write(self.path(&stored.artifact.id)?, raw)?;
        Ok(())
    }

    fn load_all(&self) -> Result<Vec<StoredArtifact>> {
        let mut out = Vec::new();
        if !self.dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Ok(a) = serde_json::from_str::<StoredArtifact>(&raw) {
                        out.push(a);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.artifact.updated_at.cmp(&a.artifact.updated_at));
        Ok(out)
    }

    /// Every artifact `caller` may read that `filter` matches, newest first.
    fn visible(&self, caller: &Caller, filter: &ArtifactFilter) -> Result<Vec<Artifact>> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|stored| stored.readable_by(caller))
            .map(|stored| stored.artifact)
            .filter(|a| filter_matches(a, filter))
            .collect())
    }
}

#[async_trait]
impl ArtifactBackend for FsArtifactBackend {
    async fn create(&self, artifact: &Artifact) -> Result<()> {
        if self.path(&artifact.id)?.exists() {
            return Err(eyre::eyre!("Artifact '{}' already exists", artifact.id));
        }
        self.write(&StoredArtifact {
            artifact: artifact.clone(),
            shares: BTreeSet::new(),
        })
    }

    async fn read(&self, id: &str, caller: &Caller) -> Result<Option<Artifact>> {
        Ok(self
            .load(id)?
            .filter(|stored| stored.readable_by(caller))
            .map(|stored| stored.artifact))
    }

    async fn update(
        &self,
        id: &str,
        content: &str,
        modified_by: &str,
        caller: &Caller,
    ) -> Result<Artifact> {
        let mut stored = self.load_owned(id, caller)?;
        let artifact = &mut stored.artifact;
        if artifact.status == ArtifactStatus::Finalized {
            return Err(eyre::eyre!(
                "Artifact '{id}' is finalized and cannot be modified"
            ));
        }
        artifact.content = content.to_string();
        artifact.last_modified_by = modified_by.to_string();
        artifact.version += 1;
        artifact.updated_at = Utc::now();
        self.write(&stored)?;
        Ok(stored.artifact)
    }

    async fn finalize(&self, id: &str, caller: &Caller) -> Result<Artifact> {
        let mut stored = self.load_owned(id, caller)?;
        stored.artifact.status = ArtifactStatus::Finalized;
        stored.artifact.updated_at = Utc::now();
        self.write(&stored)?;
        Ok(stored.artifact)
    }

    async fn delete(&self, id: &str, caller: &Caller) -> Result<()> {
        self.load_owned(id, caller)?;
        fs::remove_file(self.path(id)?)?;
        Ok(())
    }

    async fn list(&self, caller: &Caller, filter: &ArtifactFilter) -> Result<Vec<Artifact>> {
        self.visible(caller, filter)
    }

    async fn set_visibility(
        &self,
        id: &str,
        visibility: Visibility,
        caller: &Caller,
    ) -> Result<()> {
        let mut stored = self.load_owned(id, caller)?;
        stored.artifact.visibility = visibility;
        stored.artifact.updated_at = Utc::now();
        self.write(&stored)
    }

    async fn share_with(&self, id: &str, user_id: &str, caller: &Caller) -> Result<()> {
        let mut stored = self.load_owned(id, caller)?;
        if stored.shares.insert(user_id.to_string()) {
            self.write(&stored)?;
        }
        Ok(())
    }

    async fn set_owner(&self, id: &str, owner: &str) -> Result<()> {
        let mut stored = self.load_existing(id)?;
        stored.artifact.owner_id = owner.to_string();
        stored.artifact.updated_at = Utc::now();
        stored.shares.remove(owner);
        self.write(&stored)
    }

    async fn shares_of(&self, id: &str) -> Result<Vec<String>> {
        Ok(self
            .load(id)?
            .map(|stored| stored.shares.into_iter().collect())
            .unwrap_or_default())
    }

    async fn grants_of(&self, user: &str) -> Result<Vec<String>> {
        let mut ids: Vec<String> = self
            .load_all()?
            .into_iter()
            .filter(|stored| stored.shares.contains(user))
            .map(|stored| stored.artifact.id)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    async fn revoke_share(&self, id: &str, user: &str) -> Result<bool> {
        let Some(mut stored) = self.load(id)? else {
            return Ok(false);
        };
        let revoked = stored.shares.remove(user);
        if revoked {
            self.write(&stored)?;
        }
        Ok(revoked)
    }

    async fn search(
        &self,
        query: &str,
        caller: &Caller,
        filter: &ArtifactFilter,
        limit: usize,
    ) -> Result<Vec<ScoredArtifact>> {
        let q = query.to_lowercase();
        Ok(self
            .visible(caller, filter)?
            .into_iter()
            .filter(|a| {
                a.title.to_lowercase().contains(&q)
                    || a.content.to_lowercase().contains(&q)
                    || a.tags.iter().any(|t| t.contains(&q))
            })
            .take(limit)
            .map(|artifact| ScoredArtifact {
                artifact,
                score: 0.0,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    mod mirror;
    mod read_shadow;
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_file_fallback_dir_is_closed_to_the_hub() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("artifacts-v2");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o2775)).unwrap();

        FsArtifactBackend::create_at(tmp.path()).unwrap();

        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o077, 0);
    }

    fn artifact(id: &str, owner: &str, vis: Visibility) -> Artifact {
        Artifact::new(
            id,
            ArtifactType::Snippet,
            format!("Title {id}"),
            "some content about rust and databases",
            owner,
            owner,
            vis,
            vec!["Rust".into(), "rust".into(), " DB ".into()],
        )
    }

    #[test]
    fn normalize_tags_lowercases_dedups_trims() {
        let a = artifact("a", "alice", Visibility::Private);
        assert_eq!(a.tags, vec!["rust".to_string(), "db".to_string()]);
    }

    #[test]
    fn visibility_roundtrip() {
        for v in [Visibility::Private, Visibility::Team, Visibility::Public] {
            assert_eq!(v.to_string().parse::<Visibility>().unwrap(), v);
        }
        assert_eq!(
            "garbage".parse::<Visibility>().unwrap(),
            Visibility::Private
        );
    }

    // ── File backend ──

    #[tokio::test]
    async fn fs_create_read_update_finalize() {
        let tmp = tempfile::TempDir::new().unwrap();
        let be = FsArtifactBackend::create_at(tmp.path()).unwrap();
        let caller = Caller::user("agent-x");

        be.create(&artifact("s1", "agent-x", Visibility::Private))
            .await
            .unwrap();
        assert!(be
            .create(&artifact("s1", "agent-x", Visibility::Private))
            .await
            .is_err());

        let got = be.read("s1", &caller).await.unwrap().unwrap();
        assert_eq!(got.title, "Title s1");

        let up = be
            .update("s1", "new body", "agent-x", &caller)
            .await
            .unwrap();
        assert_eq!(up.version, 2);
        assert_eq!(up.content, "new body");

        be.finalize("s1", &caller).await.unwrap();
        assert!(be.update("s1", "x", "agent-x", &caller).await.is_err());
    }

    /// The id names the file; one that climbs out of the store would plant,
    /// say, a background job the hub resumes at boot.
    #[tokio::test]
    async fn fs_an_id_that_leaves_the_store_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let be = FsArtifactBackend::create_at(tmp.path()).unwrap();
        let caller = Caller::user("agent-x");
        let id = "../.local/jobs/x/manifest";
        assert!(be
            .create(&artifact(id, "agent-x", Visibility::Private))
            .await
            .is_err());
        assert!(be.read(id, &caller).await.is_err());
        assert!(be.delete(id, &caller).await.is_err());
        assert!(!tmp.path().join(".local").exists());
    }

    #[tokio::test]
    async fn fs_search_and_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let be = FsArtifactBackend::create_at(tmp.path()).unwrap();
        let caller = Caller::user("agent-x");
        be.create(&artifact("s1", "agent-x", Visibility::Private))
            .await
            .unwrap();

        let hits = be
            .search("databases", &caller, &ArtifactFilter::default(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        let filtered = be
            .list(
                &caller,
                &ArtifactFilter {
                    tag: Some("rust".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        let none = be
            .list(
                &caller,
                &ArtifactFilter {
                    tag: Some("python".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn the_file_fallback_changes_an_artifacts_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsArtifactBackend::create_at(tmp.path()).unwrap();
        s.create(&artifact("a1", "alice", Visibility::Private))
            .await
            .unwrap();
        s.set_owner("a1", "bob").await.unwrap();
        let moved = s.read("a1", &Caller::admin()).await.unwrap().unwrap();
        assert_eq!(moved.owner_id, "bob");
        assert!(s.set_owner("missing", "bob").await.is_err());
    }

    #[tokio::test]
    async fn the_file_fallback_reads_a_file_written_before_shares_were_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsArtifactBackend::create_at(tmp.path()).unwrap();
        let old = artifact("old", "alice", Visibility::Private);
        fs::write(
            tmp.path().join(FS_ARTIFACT_DIR).join("old.json"),
            serde_json::to_string_pretty(&old).unwrap(),
        )
        .unwrap();

        let read = s
            .read("old", &Caller::user("alice"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((read.title, read.version), (old.title, 1));
        assert!(s.shares_of("old").await.unwrap().is_empty());
        assert!(s.read("old", &Caller::user("bob")).await.unwrap().is_none());

        s.share_with("old", "bob", &Caller::user("alice"))
            .await
            .unwrap();
        assert!(s.read("old", &Caller::user("bob")).await.unwrap().is_some());
    }

    /// What every backend must do alike, run against each below.
    mod conformance {
        use super::*;

        pub async fn owner_reads_own_private_others_cannot(s: &dyn ArtifactBackend) {
            s.create(&artifact("p1", "alice", Visibility::Private))
                .await
                .unwrap();
            assert!(s
                .read("p1", &Caller::user("alice"))
                .await
                .unwrap()
                .is_some());
            assert!(s.read("p1", &Caller::user("bob")).await.unwrap().is_none());
            assert!(s.read("p1", &Caller::admin()).await.unwrap().is_some());
        }

        pub async fn public_readable_by_all_list_respects_acl(s: &dyn ArtifactBackend) {
            s.create(&artifact("pub1", "alice", Visibility::Public))
                .await
                .unwrap();
            s.create(&artifact("priv1", "alice", Visibility::Private))
                .await
                .unwrap();
            let bob = Caller::user("bob");
            assert!(s.read("pub1", &bob).await.unwrap().is_some());
            let listed = s.list(&bob, &ArtifactFilter::default()).await.unwrap();
            assert_eq!(
                listed.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
                ["pub1"]
            );
            let alice = Caller::user("alice");
            let all = s.list(&alice, &ArtifactFilter::default()).await.unwrap();
            assert_eq!(all.len(), 2);
        }

        pub async fn search_respects_acl(s: &dyn ArtifactBackend) {
            s.create(&artifact("s1", "alice", Visibility::Public))
                .await
                .unwrap();
            s.create(&artifact("s2", "alice", Visibility::Private))
                .await
                .unwrap();
            let hits = s
                .search(
                    "databases",
                    &Caller::user("bob"),
                    &ArtifactFilter::default(),
                    10,
                )
                .await
                .unwrap();
            assert_eq!(
                hits.iter()
                    .map(|h| h.artifact.id.as_str())
                    .collect::<Vec<_>>(),
                ["s1"]
            );
        }

        pub async fn only_the_owner_changes_an_artifact(s: &dyn ArtifactBackend) {
            s.create(&artifact("d1", "alice", Visibility::Public))
                .await
                .unwrap();
            let bob = Caller::user("bob");
            assert!(s.update("d1", "x", "bob", &bob).await.is_err());
            assert!(s.finalize("d1", &bob).await.is_err());
            assert!(s
                .set_visibility("d1", Visibility::Private, &bob)
                .await
                .is_err());
            assert!(s.share_with("d1", "carol", &bob).await.is_err());
            assert!(s.delete("d1", &bob).await.is_err());
            let unchanged = s.read("d1", &Caller::admin()).await.unwrap().unwrap();
            assert_eq!(
                (unchanged.version, unchanged.visibility),
                (1, Visibility::Public)
            );
            assert!(s.shares_of("d1").await.unwrap().is_empty());

            let alice = Caller::user("alice");
            s.set_visibility("d1", Visibility::Private, &alice)
                .await
                .unwrap();
            assert!(s.read("d1", &bob).await.unwrap().is_none());
            s.delete("d1", &alice).await.unwrap();
            assert!(s.read("d1", &Caller::admin()).await.unwrap().is_none());
            assert!(
                s.delete("d1", &alice).await.is_err(),
                "nothing is left to delete"
            );
        }

        pub async fn a_grant_lets_its_user_read_and_nothing_more(s: &dyn ArtifactBackend) {
            s.create(&artifact("g1", "alice", Visibility::Private))
                .await
                .unwrap();
            let (alice, bob) = (Caller::user("alice"), Caller::user("bob"));
            s.share_with("g1", "bob", &alice).await.unwrap();
            s.share_with("g1", "bob", &alice).await.unwrap();
            assert_eq!(s.shares_of("g1").await.unwrap(), ["bob"]);

            assert!(s.read("g1", &bob).await.unwrap().is_some());
            let listed = s.list(&bob, &ArtifactFilter::default()).await.unwrap();
            assert_eq!(
                listed.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
                ["g1"]
            );
            let hits = s
                .search("databases", &bob, &ArtifactFilter::default(), 10)
                .await
                .unwrap();
            assert_eq!(hits.len(), 1);
            assert!(s
                .read("g1", &Caller::user("carol"))
                .await
                .unwrap()
                .is_none());

            assert!(s.update("g1", "x", "bob", &bob).await.is_err());
            assert!(
                s.share_with("g1", "carol", &bob).await.is_err(),
                "a grantee cannot pass it on"
            );
            assert!(s.delete("g1", &bob).await.is_err());

            assert!(s.revoke_share("g1", "bob").await.unwrap());
            assert!(!s.revoke_share("g1", "bob").await.unwrap());
            assert!(s.read("g1", &bob).await.unwrap().is_none());
            assert!(s.grants_of("bob").await.unwrap().is_empty());
        }

        pub async fn grants_go_with_their_artifact(s: &dyn ArtifactBackend) {
            for id in ["b2", "b1"] {
                s.create(&artifact(id, "alice", Visibility::Private))
                    .await
                    .unwrap();
                s.share_with(id, "bob", &Caller::admin()).await.unwrap();
            }
            assert_eq!(s.grants_of("bob").await.unwrap(), ["b1", "b2"]);
            s.delete("b1", &Caller::user("alice")).await.unwrap();
            assert_eq!(s.grants_of("bob").await.unwrap(), ["b2"]);
            assert!(s.shares_of("b1").await.unwrap().is_empty());
            assert!(!s.revoke_share("b1", "bob").await.unwrap());
            assert!(s
                .share_with("missing", "bob", &Caller::admin())
                .await
                .is_err());
        }

        pub async fn an_artifact_changes_owner_and_its_grants_follow(s: &dyn ArtifactBackend) {
            s.create(&artifact("a1", "alice", Visibility::Private))
                .await
                .unwrap();
            let alice = Caller::user("alice");
            s.share_with("a1", "carol", &alice).await.unwrap();
            s.share_with("a1", "bob", &alice).await.unwrap();
            assert_eq!(s.shares_of("a1").await.unwrap(), ["bob", "carol"]);
            assert_eq!(s.grants_of("bob").await.unwrap(), ["a1"]);

            s.set_owner("a1", "bob").await.unwrap();
            let moved = s.read("a1", &Caller::user("bob")).await.unwrap().unwrap();
            assert_eq!(moved.owner_id, "bob");
            assert_eq!(
                s.shares_of("a1").await.unwrap(),
                ["carol"],
                "a grant to the new owner is subsumed"
            );
            assert!(s.read("a1", &alice).await.unwrap().is_none());
            assert!(s
                .read("a1", &Caller::user("carol"))
                .await
                .unwrap()
                .is_some());

            assert!(s.revoke_share("a1", "carol").await.unwrap());
            assert!(!s.revoke_share("a1", "carol").await.unwrap());
            assert!(s.grants_of("carol").await.unwrap().is_empty());
        }
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// What a [`Held`] index does to Postgres, as another writer would,
    /// just before it takes a reindex of an artifact.
    #[derive(Clone)]
    enum Meanwhile {
        /// The artifact is given new content.
        Updated(&'static str),
        /// The artifact is deleted.
        Deleted,
    }

    /// What a [`Held`] index holds of an artifact.
    #[derive(Clone, Debug, PartialEq)]
    struct Vector {
        tags: Vec<String>,
        content: String,
        hash: String,
    }

    /// An index kept in memory: what it holds, whether it is marked
    /// complete, and what it was asked, in order. It fails, hangs, and
    /// changes artifacts under a reindex, as told.
    #[derive(Default)]
    struct Held {
        held: std::sync::Mutex<std::collections::BTreeMap<String, Vector>>,
        complete: AtomicBool,
        asked: std::sync::Mutex<Vec<String>>,
        /// How many more times a reindex of an id is refused.
        failing: std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
        /// Writes fail as an index that cannot embed just now does, while
        /// everything else is answered: its marker read, its counts.
        overloaded: AtomicBool,
        /// Ids whose reindex always fails: a 500 the index answers to this
        /// one object, while it takes every other.
        broken: std::sync::Mutex<BTreeSet<String>>,
        /// Every write fails, while it answers everything else: as Weaviate
        /// does with its shards read-only.
        read_only: AtomicBool,
        /// Times it was given an artifact to index, however that went.
        given: AtomicUsize,
        /// Nothing answers: every request fails.
        down: AtomicBool,
        /// Searches alone fail.
        deaf: AtomicBool,
        /// A reindex never returns, as on a hub killed before it could.
        stuck: AtomicBool,
        /// Reindexes that have come in and will never return.
        stuck_now: AtomicUsize,
        /// Hashes are not kept, as by an index that has nowhere for them.
        hashless: AtomicBool,
        /// A search never returns, as from a node that accepts the
        /// question and does not answer.
        hung: AtomicBool,
        searches: AtomicUsize,
        /// What a search of it answers, best first; nothing unless set.
        ranking: std::sync::Mutex<Vec<(String, f32)>>,
        /// Pages whose text had to be read to tell what differed.
        pages_read: AtomicUsize,
        /// Times the ids held were listed.
        listings: AtomicUsize,
        /// A tag the tagger adds to every artifact.
        tagging: std::sync::Mutex<Option<&'static str>>,
        /// Done once, as a reindex of the id comes in.
        meanwhile: std::sync::Mutex<std::collections::BTreeMap<String, Meanwhile>>,
        /// Ids given new content every time a reindex of them comes in.
        churning: std::sync::Mutex<BTreeSet<String>>,
        churned: AtomicUsize,
        pool: std::sync::Mutex<Option<PostgresPool>>,
        /// Decides, of each write (the id, whether it is held already),
        /// whether it fails: for an index that fails by a rule of its own
        /// and not for what the artifact is.
        #[allow(clippy::type_complexity)]
        script: std::sync::Mutex<Option<Box<dyn FnMut(&str, bool) -> bool + Send>>>,
        /// Writes are taken and kept back, to land and to be answered when
        /// the test says, in the order it says ([`Held::land`],
        /// [`Held::answer`]): as a slow index, or one whose answer comes
        /// after whoever asked has given up.
        keeping_back: AtomicBool,
        kept_back: std::sync::Mutex<Vec<KeptBack>>,
        /// Counts given too many, this many times: as a count read a
        /// moment after a write that Postgres was read before.
        miscounts: AtomicUsize,
        /// Times it was asked what differs of a page.
        compared: AtomicUsize,
        /// Why it is not the index the hub made, as a write would find.
        not_ours: std::sync::Mutex<Option<String>>,
        /// The marker is lost while the next search is being answered: as
        /// a collection dropped and made again under it.
        lost_while_searched: AtomicBool,
        /// Ids whose write fails saying this much: a page of an error.
        long_winded: std::sync::Mutex<BTreeSet<String>>,
        /// The lock of the compare under way is lost as the index is
        /// asked what differs.
        lock_lost_comparing: AtomicBool,
        /// Times it was asked whether it is marked compared.
        completes: AtomicUsize,
        /// Times it was asked how many it holds.
        counts: AtomicUsize,
        /// It will not be marked compared: a compare walks every artifact
        /// and is refused at its end.
        refuses_marking: AtomicBool,
        /// Its count alone fails, its marker answering: as Weaviate down,
        /// whose marker is no request.
        counts_fail: AtomicBool,
        /// While set, tagging waits for a permit from it: the write's own
        /// tags are written back only when a test lets them.
        tag_gate: std::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        /// Asked whether it is marked compared, it panics: as a keeper's
        /// round that dies.
        panics_on_look: AtomicBool,
        /// Asked whether it is marked compared, it never answers: as a
        /// keeper's round stuck on a call nothing bounds.
        look_hangs: AtomicBool,
        /// Each artifact it is given takes this long to take.
        slow: std::sync::Mutex<Option<std::time::Duration>>,
        /// Each page a compare asks of it takes this long to answer.
        slow_pages: std::sync::Mutex<Option<std::time::Duration>>,
    }

    /// A write an index has taken and not yet let land, or not yet
    /// answered.
    struct KeptBack {
        id: String,
        vector: Vector,
        landed: bool,
        answer: Option<tokio::sync::oneshot::Sender<Result<()>>>,
    }

    impl Held {
        fn on(pool: &PostgresPool) -> Arc<Self> {
            let held = Self::default();
            *held.pool.lock().unwrap() = Some(pool.clone());
            Arc::new(held)
        }

        fn is_complete(&self) -> bool {
            self.complete.load(Ordering::SeqCst)
        }

        /// The writes kept back, oldest first: each its place and its id.
        fn kept_back(&self) -> Vec<(usize, String)> {
            let kept = self.kept_back.lock().unwrap();
            let waiting = kept.iter().enumerate();
            waiting.map(|(at, write)| (at, write.id.clone())).collect()
        }

        /// Wait until `n` writes have come in and been kept back.
        async fn until_kept_back(&self, n: usize) {
            let begun = std::time::Instant::now();
            while self.kept_back.lock().unwrap().len() < n {
                assert!(
                    begun.elapsed() < std::time::Duration::from_secs(10),
                    "no write came in to be kept back"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }

        /// Let the write at `at` land: the index holds it from now on.
        /// Whoever sent it is not answered yet.
        fn land(&self, at: usize) {
            let mut kept = self.kept_back.lock().unwrap();
            let write = &mut kept[at];
            write.landed = true;
            let (id, vector) = (write.id.clone(), write.vector.clone());
            self.held.lock().unwrap().insert(id, vector);
        }

        /// Answer whoever sent the write at `at`, if they still wait:
        /// taken, or failed.
        fn answer(&self, at: usize, taken: bool) {
            let mut kept = self.kept_back.lock().unwrap();
            if let Some(answer) = kept[at].answer.take() {
                let _ = answer.send(match taken {
                    true => Ok(()),
                    false => Err(eyre::eyre!("503: answered late, and with a failure")),
                });
            }
        }

        fn up(&self) -> Result<()> {
            if self.down.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("the vector database is down"));
            }
            Ok(())
        }

        fn content(&self, id: &str) -> Option<String> {
            let held = self.held.lock().unwrap();
            held.get(id).map(|vector| vector.content.clone())
        }

        fn ids_held(&self) -> Vec<String> {
            self.held.lock().unwrap().keys().cloned().collect()
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }

        fn reindexed(&self) -> Vec<String> {
            let asked = self.asked().into_iter();
            asked
                .filter_map(|asked| asked.strip_prefix("reindex ").map(str::to_string))
                .collect()
        }

        /// Hold `content` for `id`, under a hash that is not its own: as an
        /// index restored to an earlier state holds an earlier text.
        fn hold(&self, id: &str, content: &str) {
            self.held.lock().unwrap().insert(
                id.to_string(),
                Vector {
                    tags: Vec::new(),
                    content: content.to_string(),
                    hash: format!("hash of {content}"),
                },
            );
        }

        async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) {
            let pool = self.pool.lock().unwrap().clone().expect("a pool");
            let conn = pool.get().await.unwrap();
            conn.execute(sql, params).await.unwrap();
        }
    }

    #[async_trait]
    impl ArtifactIndexer for Held {
        async fn tags(&self, artifact: &Artifact) -> Vec<String> {
            let gate = self.tag_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.expect("never closed").forget();
            }
            let added = *self.tagging.lock().unwrap();
            let mut tags = artifact.tags.clone();
            tags.extend(added.map(str::to_string));
            normalize_tags(tags)
        }
        async fn remove(&self, id: String) -> Result<()> {
            self.up()?;
            if self.read_only.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("500: the store is read-only"));
            }
            self.asked.lock().unwrap().push(format!("remove {id}"));
            self.held.lock().unwrap().remove(&id);
            Ok(())
        }
        async fn search(&self, _: &str, _: usize) -> Result<Vec<(String, f32)>> {
            self.up()?;
            self.searches.fetch_add(1, Ordering::SeqCst);
            if self.hung.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.deaf.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("the search timed out"));
            }
            if self.lost_while_searched.swap(false, Ordering::SeqCst) {
                self.complete.store(false, Ordering::SeqCst);
            }
            Ok(self.ranking.lock().unwrap().clone())
        }
        async fn reindex(&self, artifact: &Artifact, text_hash: &str) -> Result<()> {
            self.given.fetch_add(1, Ordering::SeqCst);
            self.up()?;
            let slow = *self.slow.lock().unwrap();
            if let Some(slow) = slow {
                tokio::time::sleep(slow).await;
            }
            if self.stuck.load(Ordering::SeqCst) {
                self.stuck_now.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            self.asked
                .lock()
                .unwrap()
                .push(format!("reindex {}", artifact.id));
            if self.read_only.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("500: the store is read-only"));
            }
            if self.long_winded.lock().unwrap().contains(&artifact.id) {
                let said = "the index explains at length why it did not take this. ".repeat(200);
                return Err(eyre::eyre!("500: {said}"));
            }
            if self.broken.lock().unwrap().contains(&artifact.id) {
                return Err(eyre::eyre!(
                    "500: internal error while embedding this object"
                ));
            }
            if self.overloaded.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("503: the model is loading"));
            }
            if let Some(left) = self.failing.lock().unwrap().get_mut(&artifact.id) {
                if *left > 0 {
                    *left -= 1;
                    return Err(eyre::eyre!("400: the index will not take it"));
                }
            }
            if self.churning.lock().unwrap().contains(&artifact.id) {
                let round = self.churned.fetch_add(1, Ordering::SeqCst);
                self.execute(
                    "UPDATE artifacts SET content = $2 WHERE id = $1",
                    &[&artifact.id, &format!("round {round}")],
                )
                .await;
            }
            let meanwhile = self.meanwhile.lock().unwrap().remove(&artifact.id);
            match meanwhile {
                Some(Meanwhile::Updated(content)) => {
                    self.execute(
                        "UPDATE artifacts SET content = $2 WHERE id = $1",
                        &[&artifact.id, &content],
                    )
                    .await;
                }
                Some(Meanwhile::Deleted) => {
                    self.execute("DELETE FROM artifacts WHERE id = $1", &[&artifact.id])
                        .await;
                }
                None => {}
            }
            {
                let held = self.held.lock().unwrap().contains_key(&artifact.id);
                if let Some(script) = self.script.lock().unwrap().as_mut() {
                    if script(&artifact.id, held) {
                        return Err(eyre::eyre!(
                            "503: failed by the index's own rule, not for this artifact"
                        ));
                    }
                }
            }
            if self.keeping_back.load(Ordering::SeqCst) {
                let (answer, answered) = tokio::sync::oneshot::channel();
                self.kept_back.lock().unwrap().push(KeptBack {
                    id: artifact.id.clone(),
                    vector: Vector {
                        tags: artifact.tags.clone(),
                        content: artifact.content.clone(),
                        hash: text_hash.to_string(),
                    },
                    landed: false,
                    answer: Some(answer),
                });
                return answered
                    .await
                    .unwrap_or_else(|_| Err(eyre::eyre!("the index went away")));
            }
            // The other writer's own index write came first; this one,
            // of what was read before it, lands last.
            self.held.lock().unwrap().insert(
                artifact.id.clone(),
                Vector {
                    tags: artifact.tags.clone(),
                    content: artifact.content.clone(),
                    hash: text_hash.to_string(),
                },
            );
            Ok(())
        }
        async fn complete(&self) -> Result<bool> {
            self.completes.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.panics_on_look.load(Ordering::SeqCst),
                "the index's look panicked"
            );
            if self.look_hangs.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.up()?;
            Ok(self.is_complete())
        }
        async fn mark_incomplete(&self) -> Result<()> {
            self.up()?;
            self.asked.lock().unwrap().push("mark incomplete".into());
            self.complete.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn mark_complete(&self) -> Result<()> {
            self.up()?;
            if self.refuses_marking.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("500: the marker will not be written"));
            }
            self.asked.lock().unwrap().push("mark complete".into());
            self.complete.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn differing(&self, page: &[(String, String)]) -> Result<Vec<String>> {
            self.up()?;
            let slow = *self.slow_pages.lock().unwrap();
            if let Some(slow) = slow {
                tokio::time::sleep(slow).await;
            }
            self.compared.fetch_add(1, Ordering::SeqCst);
            if self.lock_lost_comparing.swap(false, Ordering::SeqCst) {
                // With a timeout, it waits for the backend to be gone and its
                // lock with it: the next repair is not refused by a lock that
                // is on its way out.
                self.execute(
                    "SELECT pg_terminate_backend(pid, 5000) FROM pg_locks \
                     WHERE locktype = 'advisory' AND pid <> pg_backend_pid() \
                       AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                       AND (classid::bigint << 32) | objid::bigint = $1",
                    &[&BACKFILL_LOCK],
                )
                .await;
            }
            let hashless = self.hashless.load(Ordering::SeqCst);
            let held = self.held.lock().unwrap();
            let differing: Vec<String> = page
                .iter()
                .filter(|(id, hash)| match held.get(id) {
                    Some(vector) => !hashless && vector.hash != *hash,
                    None => true,
                })
                .map(|(id, _)| id.clone())
                .collect();
            if !differing.is_empty() {
                // Only a page that differs has its text read.
                self.pages_read.fetch_add(1, Ordering::SeqCst);
            }
            Ok(differing)
        }
        fn compares_text(&self) -> bool {
            !self.hashless.load(Ordering::SeqCst)
        }
        fn refused_index(&self) -> Option<String> {
            self.not_ours.lock().unwrap().clone()
        }
        async fn count(&self) -> Result<usize> {
            self.counts.fetch_add(1, Ordering::SeqCst);
            self.up()?;
            if self.counts_fail.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("the count timed out"));
            }
            let over = self
                .miscounts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            Ok(self.held.lock().unwrap().len() + usize::from(over))
        }
        async fn ids_after(
            &self,
            after: Option<String>,
            limit: usize,
        ) -> Result<(Vec<String>, Option<String>)> {
            self.up()?;
            self.listings.fetch_add(1, Ordering::SeqCst);
            let held = self.held.lock().unwrap();
            let ids: Vec<String> = held
                .keys()
                .filter(|id| after.as_ref().is_none_or(|after| *id > after))
                .take(limit)
                .cloned()
                .collect();
            let next = (ids.len() == limit).then(|| ids.last().cloned()).flatten();
            Ok((ids, next))
        }
    }

    /// A store over `pool` indexing into `index`.
    async fn store_on(pool: &PostgresPool, index: &Arc<Held>) -> Arc<KnowledgeStore> {
        let index = Arc::clone(index) as Arc<dyn ArtifactIndexer>;
        let store = KnowledgeStore::new(pool.clone(), Some(index)).await;
        let store = store.unwrap();
        *store.index.pace.lock().unwrap() = QUICK;
        store
    }

    async fn on_indexed_store<F, Fut>(test: F)
    where
        F: FnOnce(Arc<KnowledgeStore>, Arc<Held>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            KnowledgeStore::build_pool,
            |pool| async move {
                let index = Held::on(&pool);
                let store = store_on(&pool, &index).await;
                test(store, index).await;
            },
        )
        .await;
    }

    /// Put `n` artifacts straight into Postgres, noted as pending like any
    /// write, with no task indexing them.
    async fn stored(store: &KnowledgeStore, n: usize) {
        for n in 0..n {
            let artifact = artifact(&format!("a{n:03}"), "alice", Visibility::Public);
            store.insert_row(&artifact).await.unwrap();
        }
    }

    /// Run `sql` as a hub of a release before writes were noted would
    /// have: the trigger that notes them is off meanwhile.
    async fn unnoted(store: &KnowledgeStore, sql: &[&str]) {
        let conn = store.pool.get().await.unwrap();
        conn.batch_execute("ALTER TABLE artifacts DISABLE TRIGGER artifacts_note_index_pending")
            .await
            .unwrap();
        for sql in sql {
            conn.batch_execute(sql).await.unwrap();
        }
        conn.batch_execute("ALTER TABLE artifacts ENABLE TRIGGER artifacts_note_index_pending")
            .await
            .unwrap();
    }

    /// Wait until `met`, for at most ten seconds.
    async fn until(what: &str, met: impl Fn() -> bool) {
        let begun = std::time::Instant::now();
        while !met() {
            assert!(
                begun.elapsed() < std::time::Duration::from_secs(10),
                "not within ten seconds: {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// The artifacts noted as pending, with the hash noted and the tries.
    async fn noted(store: &KnowledgeStore) -> Vec<(String, String, i32)> {
        let conn = store.pool.get().await.unwrap();
        let rows = conn
            .query(
                "SELECT artifact_id, text_hash, tries FROM artifact_index_pending ORDER BY artifact_id",
                &[],
            )
            .await
            .unwrap();
        rows.iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect()
    }

    async fn pending(store: &KnowledgeStore) -> Vec<String> {
        let noted = noted(store).await;
        noted.into_iter().map(|(id, _, _)| id).collect()
    }

    /// Wait until nothing is pending, for at most ten seconds.
    async fn until_none_pending(store: &KnowledgeStore) {
        let begun = std::time::Instant::now();
        while !pending(store).await.is_empty() {
            assert!(begun.elapsed() < std::time::Duration::from_secs(10));
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Whether the index holds exactly what is stored, by id and content.
    async fn agrees(store: &KnowledgeStore, index: &Held) -> bool {
        let conn = store.pool.get().await.unwrap();
        let rows = conn
            .query("SELECT id, content FROM artifacts ORDER BY id", &[])
            .await
            .unwrap();
        let stored: Vec<(String, String)> =
            rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        let held = index.held.lock().unwrap();
        let held: Vec<(String, String)> = held
            .iter()
            .map(|(id, vector)| (id.clone(), vector.content.clone()))
            .collect();
        stored == held
    }

    /// The keeper's pace in these tests: no wait before an artifact the
    /// index did not take is tried again, so that the next round tries it.
    const QUICK: IndexPace = IndexPace {
        backoff: std::time::Duration::ZERO,
        retry: std::time::Duration::from_millis(20),
        watch: std::time::Duration::from_secs(3600),
        reconcile: std::time::Duration::from_secs(3600),
        ..IndexPace::DEFAULT
    };

    /// Before a new index is filled, search goes by keyword and every
    /// artifact is found; a compare notes each one, they are indexed, and
    /// search relies on the index after it. A repair of an index that
    /// agrees with Postgres indexes nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_index_is_filled_and_search_waits_for_it() {
        on_indexed_store(|store, index| async move {
            stored(&store, 250).await;
            let filter = ArtifactFilter::default();
            let found = store
                .search("databases", &Caller::user("bob"), &filter, 5)
                .await
                .unwrap();
            assert_eq!(
                found.len(),
                5,
                "found by keyword while the index is incomplete"
            );

            let pages = std::sync::Mutex::new(Vec::new());
            let done = store
                .backfill(false, |done, total| {
                    pages.lock().unwrap().push((done, total))
                })
                .await
                .unwrap();
            assert_eq!((done.indexed, done.total, done.failed), (250, 250, 0));
            assert_eq!(*pages.lock().unwrap(), [(250, 250)]);
            assert!(agrees(&store, &index).await);
            assert!(index.is_complete());
            assert!(pending(&store).await.is_empty(), "each taken off the list");
            let found = store
                .search("databases", &Caller::user("bob"), &filter, 5)
                .await
                .unwrap();
            assert!(found.is_empty(), "the index is relied on once complete");
            let state = store.index_state();
            assert_eq!(state.complete, Some(true));
            assert_eq!(state.last_backfill, Some(done));
            assert!(state.backfill.is_none() && state.reconciled_at.is_some());

            index.asked.lock().unwrap().clear();
            let again = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!(again, Backfilled::default());
            assert!(index.asked().is_empty(), "nothing differs, nothing done");
        })
        .await;
    }

    /// A compare of an index not yet marked unmarks it first, and marks it
    /// once what differs is noted: compared with Postgres, so that it and
    /// the pending list between them account for every artifact. What it
    /// noted is indexed after, and an artifact the index will not take
    /// stays pending without the mark being taken back. An index that
    /// does not answer is not compared, and not marked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_compare_marks_the_index_once_what_differs_is_noted() {
        on_indexed_store(|store, index| async move {
            stored(&store, 5).await;
            index
                .failing
                .lock()
                .unwrap()
                .insert("a002".into(), usize::MAX);

            let done = store.backfill(false, |_, _| {}).await.unwrap();
            let asked = index.asked();
            assert_eq!(asked[0], "mark incomplete", "before anything else");
            assert_eq!(asked[1], "mark complete", "and before anything is indexed");
            assert!(index.is_complete());
            assert_eq!((done.indexed, done.total, done.failed), (4, 5, 1));
            assert_eq!(done.failures.len(), 1);
            assert_eq!(done.failures[0].id, "a002");
            assert!(done.failures[0].error.contains("will not take it"));
            assert_eq!(index.ids_held(), ["a000", "a001", "a003", "a004"]);
            assert_eq!(store.index_state().complete, Some(true));
            assert_eq!(pending(&store).await, ["a002"], "still to index");

            // One that does not answer at all: nothing compared or marked.
            index.mark_incomplete().await.unwrap();
            index.down.store(true, Ordering::SeqCst);
            assert!(store.backfill(false, |_, _| {}).await.is_err());
            index.down.store(false, Ordering::SeqCst);
            assert!(!index.is_complete());
        })
        .await;
    }

    /// An artifact the index failed on stays pending, and goes in the next
    /// time the pending list is worked.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_artifact_that_fails_once_is_tried_again_after_the_rest() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            index.failing.lock().unwrap().insert("a000".into(), 1);
            let done = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!((done.indexed, done.failed), (2, 1));
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert_eq!(index.reindexed(), ["a000", "a001", "a002", "a000"]);
            assert!(index.is_complete());
        })
        .await;
    }

    /// An artifact updated, or deleted, between a repair's read of it and
    /// the index taking that read ends as Postgres has it: the newer text,
    /// or no vector.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_artifact_changed_under_a_repair_ends_as_postgres_has_it() {
        on_indexed_store(|store, index| async move {
            stored(&store, 4).await;
            {
                let mut meanwhile = index.meanwhile.lock().unwrap();
                meanwhile.insert("a001".into(), Meanwhile::Updated("newer content"));
                meanwhile.insert("a002".into(), Meanwhile::Deleted);
            }
            let done = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!(index.content("a001").as_deref(), Some("newer content"));
            assert_eq!(index.content("a002"), None, "not indexed once deleted");
            assert!(agrees(&store, &index).await);
            assert_eq!((done.indexed, done.removed), (3, 1));
            assert!(index.is_complete());
            // Each was noted again as it changed, by a number the try that
            // indexed it had not read: it stays, for one more try that
            // finds nothing to do.
            assert_eq!(pending(&store).await, ["a001", "a002"]);
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert!(agrees(&store, &index).await);
        })
        .await;
    }

    /// An artifact that never stops changing is given up, not indexed for
    /// ever.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_artifact_that_keeps_changing_is_counted_as_failed() {
        on_indexed_store(|store, index| async move {
            stored(&store, 1).await;
            index.churning.lock().unwrap().insert("a000".into());
            let failed = store
                .settle(index.as_ref(), "a000")
                .await
                .unwrap_err()
                .to_string();
            assert!(failed.contains("kept changing"), "{failed}");
            assert_eq!(index.churned.load(Ordering::SeqCst), SETTLE_TRIES);
        })
        .await;
    }

    /// Every noting of an artifact takes a number of its own and starts
    /// again: no tries, no error, due at once, whatever the last one had
    /// come to (DW-620 invariant 3, at the trigger). A write that changes
    /// nothing indexed notes nothing. A row put in without a number, as a
    /// hub of the release before does, is given one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_noting_takes_a_number_of_its_own_and_is_due_at_once() {
        on_indexed_store(|store, _index| async move {
            stored(&store, 1).await;
            let conn = store.pool.get().await.unwrap();
            let row = || async {
                let read = "SELECT noted_seq, tries, last_error, first_failed_at IS NULL, \
                            next_try_at IS NULL FROM artifact_index_pending WHERE artifact_id = 'a000'";
                let row = conn.query_one(read, &[]).await.unwrap();
                let (seq, tries): (i64, i32) = (row.get(0), row.get(1));
                let error: Option<String> = row.get(2);
                let (unfailed, due_now): (bool, bool) = (row.get(3), row.get(4));
                (seq, tries, error, unfailed, due_now)
            };
            let first = row().await;
            assert_eq!((first.1, &first.2, first.3, first.4), (0, &None, true, true));

            // As the index failing to take it leaves it.
            let failing = "UPDATE artifact_index_pending SET tries = 3, last_error = 'no', \
                           first_failed_at = now(), next_try_at = now() + interval '1 hour'";
            conn.execute(failing, &[]).await.unwrap();
            assert_eq!(row().await.0, first.0, "a failure is not a noting");

            let unchanged = "UPDATE artifacts SET updated_at = now() WHERE id = 'a000'";
            conn.execute(unchanged, &[]).await.unwrap();
            let still = row().await;
            assert_eq!((still.0, still.1, still.4), (first.0, 3, false), "nothing noted");

            let changed = "UPDATE artifacts SET content = 'another text' WHERE id = 'a000'";
            conn.execute(changed, &[]).await.unwrap();
            let second = row().await;
            assert!(second.0 > first.0, "a number of its own: {second:?}");
            assert_eq!((second.1, &second.2, second.3, second.4), (0, &None, true, true));

            // Changed back: the hash it had, and still another number.
            let back = "UPDATE artifacts SET content = (SELECT content FROM artifacts \
                        WHERE id = 'a000') WHERE id = 'a000'";
            conn.execute(back, &[]).await.unwrap();
            assert_eq!(row().await.0, second.0, "the same text is no noting");
            conn.execute("DELETE FROM artifacts WHERE id = 'a000'", &[])
                .await
                .unwrap();
            let third = row().await;
            assert!(third.0 > second.0, "a removal is a noting: {third:?}");

            // As a hub of the release before notes a failure: no number given.
            let old = "INSERT INTO artifact_index_pending (artifact_id, text_hash, tries) \
                       VALUES ('from-before', 'h', 1) RETURNING noted_seq";
            let given: i64 = conn.query_one(old, &[]).await.unwrap().get(0);
            assert!(given > third.0);
        })
        .await;
    }

    /// One repair at a time, across every connection to the database: a
    /// second is told so, or waits its turn; and the turn is given up
    /// however the first ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_repair_runs_at_a_time() {
        on_indexed_store(|store, index| async move {
            stored(&store, 1).await;
            let first = BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false)
                .await
                .unwrap();
            let told = std::time::Duration::from_secs(30);
            let busy = tokio::time::timeout(told, store.backfill(false, |_, _| {}))
                .await
                .expect("told at once, not kept waiting")
                .unwrap_err();
            assert!(busy.downcast_ref::<BackfillBusy>().is_some(), "{busy}");
            assert!(index.asked().is_empty(), "nothing touched");

            let waiting = {
                let store = store.clone();
                tokio::spawn(async move { store.backfill(true, |_, _| {}).await })
            };
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert!(!waiting.is_finished(), "waits its turn");
            drop(first);
            let waited = tokio::time::timeout(std::time::Duration::from_secs(30), waiting)
                .await
                .expect("its turn comes once the first is done");
            assert_eq!(waited.unwrap().unwrap().indexed, 1);

            // A repair cut short lets go too.
            let held = {
                let pool = store.pool.clone();
                tokio::spawn(async move {
                    let _held = BackfillLock::acquire(&pool, BACKFILL_LOCK, false)
                        .await
                        .unwrap();
                    std::future::pending::<()>().await
                })
            };
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert!(BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false)
                .await
                .is_err());
            held.abort();
            let begun = std::time::Instant::now();
            while BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false)
                .await
                .is_err()
            {
                assert!(begun.elapsed() < std::time::Duration::from_secs(10));
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    /// A repair that ends has let the lock go itself, and has not left it
    /// to go with its connection: a session's locks last until Postgres
    /// notices the session closed, and here that moment never ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repair_lets_the_lock_go_before_its_connection_does() {
        on_indexed_store(|store, index| async move {
            stored(&store, 1).await;
            let kept = Arc::new(std::sync::Mutex::new(Vec::new()));
            KEPT_OPEN
                .scope(Arc::clone(&kept), async {
                    assert_eq!(store.backfill(false, |_, _| {}).await.unwrap().indexed, 1);
                    assert_eq!(kept.lock().unwrap().len(), 1, "its connection is open yet");
                    let next = store.backfill(false, |_, _| {}).await;
                    assert!(next.is_ok(), "the next repair is not kept out: {next:?}");
                })
                .await;
            assert!(index.is_complete());
        })
        .await;
    }

    /// A compare whose lock is lost with its connection stops, and does
    /// not mark the index compared on the word of a lock another process
    /// may hold by then.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repair_that_loses_its_lock_stops_without_marking_the_index() {
        on_indexed_store(|store, index| async move {
            stored(&store, 2).await;
            index.lock_lost_comparing.store(true, Ordering::SeqCst);
            let lost = store.backfill(false, |_, _| {}).await.unwrap_err();
            assert!(format!("{lost:#}").contains("the lock with it"), "{lost:#}");
            assert!(!index.is_complete());
            assert!(!index.asked().contains(&"mark complete".to_string()));
            let state = store.index_state();
            assert_eq!(state.complete, Some(false));
            assert!(state.last_error.unwrap().contains("lock"));

            // The lock went with the connection: the next repair has it.
            let done = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!(done.failed, 0);
            assert!(index.is_complete());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repair_with_no_index_says_so() {
        with_store(|store| async move {
            let none = store.backfill(false, |_, _| {}).await.unwrap_err();
            assert!(
                none.to_string().contains("no artifact index is configured"),
                "{none}"
            );
            assert!(!store.index_state().configured);
        })
        .await;
    }

    /// Every write that changes what is indexed of an artifact is noted as
    /// pending by Postgres itself, in the write's own transaction, with the
    /// hash of what is to be indexed: whoever writes, and whether or not
    /// anything goes on to index it. A write that changes nothing indexed
    /// is not noted.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_write_is_noted_as_pending_in_its_own_transaction() {
        with_store(|store| async move {
            let alice = Caller::user("alice");
            let hash = |id: &'static str| {
                let store = store.clone();
                async move { store.stored_hash(id).await.unwrap() }
            };
            let clear = || async {
                let conn = store.pool.get().await.unwrap();
                conn.execute("DELETE FROM artifact_index_pending", &[])
                    .await
                    .unwrap();
            };

            store
                .create(&artifact("made", "alice", Visibility::Private))
                .await
                .unwrap();
            assert!(store
                .seed(&artifact("seeded", "alice", Visibility::Private))
                .await
                .unwrap());
            assert_eq!(
                noted(&store).await,
                [
                    ("made".to_string(), hash("made").await, 0),
                    ("seeded".to_string(), hash("seeded").await, 0)
                ]
            );
            assert_eq!(hash("made").await.len(), 32, "an md5, as hex");
            assert_ne!(hash("made").await, hash("seeded").await, "titles differ");

            clear().await;
            let before = hash("made").await;
            store
                .update("made", "changed", "alice", &alice)
                .await
                .unwrap();
            let changed = hash("made").await;
            assert_ne!(changed, before);
            assert_eq!(noted(&store).await, [("made".to_string(), changed, 0)]);

            // A tag write, as the tagger's is, and one from outside the hub.
            clear().await;
            let conn = store.pool.get().await.unwrap();
            conn.execute(
                "UPDATE artifacts SET tags = '[\"ops\"]' WHERE id = 'made'",
                &[],
            )
            .await
            .unwrap();
            conn.execute(
                "UPDATE artifacts SET title = 'Renamed' WHERE id = 'seeded'",
                &[],
            )
            .await
            .unwrap();
            assert_eq!(pending(&store).await, ["made", "seeded"]);
            assert_eq!(noted(&store).await[0].1, hash("made").await);

            // Nothing indexed changes: nothing noted.
            clear().await;
            store.finalize("seeded", &alice).await.unwrap();
            store
                .set_visibility("seeded", Visibility::Public, &alice)
                .await
                .unwrap();
            store.share_with("made", "bob", &alice).await.unwrap();
            store.set_owner("made", "carol").await.unwrap();
            assert!(noted(&store).await.is_empty());

            // A delete is noted with no hash; a write rolled back, not at all.
            store.delete("seeded", &alice).await.unwrap();
            assert_eq!(
                noted(&store).await,
                [("seeded".to_string(), String::new(), 0)]
            );
            clear().await;
            let mut conn = store.pool.get().await.unwrap();
            let rolled_back = conn.transaction().await.unwrap();
            rolled_back
                .execute(
                    "UPDATE artifacts SET content = 'never' WHERE id = 'made'",
                    &[],
                )
                .await
                .unwrap();
            rolled_back.rollback().await.unwrap();
            assert!(
                noted(&store).await.is_empty(),
                "noted with the write, or not"
            );
        })
        .await;
    }

    /// B1 (a): a hub killed after an artifact is committed and before the
    /// index has it. The write is noted already, so the next hub's first
    /// round indexes it, with no repair of the index.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_artifact_committed_by_a_hub_killed_before_indexing_it_is_indexed_by_the_next() {
        workforce_postgres::test_support::with_test_pool(
            KnowledgeStore::build_pool,
            |pool| async move {
                let index = Held::on(&pool);
                index.mark_complete().await.unwrap();
                let killed = store_on(&pool, &index).await;
                index.stuck.store(true, Ordering::SeqCst);
                killed
                    .create(&artifact("orphan", "alice", Visibility::Public))
                    .await
                    .unwrap();
                assert_eq!(pending(&killed).await, ["orphan"], "noted with the commit");
                // Killed with the reindex under way, not before it began:
                // one that had yet to begin would go through once the
                // index answers again, beside the next hub's.
                until("the reindex under way", || {
                    index.stuck_now.load(Ordering::SeqCst) > 0
                })
                .await;
                drop(killed);
                assert!(index.is_complete() && index.content("orphan").is_none());

                index.stuck.store(false, Ordering::SeqCst);
                index.asked.lock().unwrap().clear();
                let next = store_on(&pool, &index).await;
                next.keep_index_complete(QUICK);
                until("indexed", || index.content("orphan").is_some()).await;
                until_none_pending(&next).await;
                assert!(index.is_complete());
                assert!(agrees(&next, &index).await);
                assert_eq!(index.reindexed(), ["orphan"]);
            },
        )
        .await;
    }

    /// B1 (c): artifacts written, changed and deleted while the hub indexed
    /// elsewhere, or ran a release that noted nothing. Nothing is pending,
    /// the marker still matches and the counts agree; comparing the index
    /// with Postgres artifact by artifact finds all three, which a hub does
    /// when it starts.
    #[tokio::test(flavor = "multi_thread")]
    async fn artifacts_written_while_the_hub_indexed_elsewhere_are_found_by_comparing() {
        workforce_postgres::test_support::with_test_pool(
            KnowledgeStore::build_pool,
            |pool| async move {
                let index = Held::on(&pool);
                let before = store_on(&pool, &index).await;
                stored(&before, 3).await;
                before.backfill(false, |_, _| {}).await.unwrap();
                drop(before);

                let elsewhere = KnowledgeStore::new(pool.clone(), None).await.unwrap();
                unnoted(
                    &elsewhere,
                    &[
                        "UPDATE artifacts SET content = 'changed elsewhere' WHERE id = 'a000'",
                        "DELETE FROM artifacts WHERE id = 'a001'",
                        "INSERT INTO artifacts SELECT 'a009', artifact_type, title, content, owner_id, \
                         visibility, tags, status, version, created_by, last_modified_by, created_at, \
                         updated_at FROM artifacts WHERE id = 'a002'",
                    ],
                )
                .await;
                assert!(pending(&elsewhere).await.is_empty());
                drop(elsewhere);

                let back = store_on(&pool, &index).await;
                assert!(index.is_complete());
                assert_eq!(index.count().await.unwrap(), 3, "as many as are stored");
                index.asked.lock().unwrap().clear();
                // A round of a store that compared at its start, and well:
                // the first round of `back`, still to come, compares.
                let cheap = store_on(&pool, &index).await;
                cheap.ended((cheap.clock)(), true);
                assert!(cheap.index_round(index.as_ref()).await.unwrap());
                assert!(index.asked().is_empty(), "nothing a cheap round can see");
                assert!(!agrees(&back, &index).await);
                drop(cheap);

                back.keep_index_complete(QUICK);
                // And the repair recorded, which it is a moment after the
                // index is marked complete.
                until("put right", || {
                    index.is_complete()
                        && index.content("a000").as_deref() == Some("changed elsewhere")
                        && index.ids_held() == ["a000", "a002", "a009"]
                        && back.index_state().last_backfill.is_some()
                })
                .await;
                assert!(agrees(&back, &index).await);
                let mut reindexed = index.reindexed();
                reindexed.sort();
                assert_eq!(reindexed, ["a000", "a009"], "only what differed");
                let last = back.index_state().last_backfill.unwrap();
                assert_eq!((last.indexed, last.removed, last.total), (2, 1, 3));
            },
        )
        .await;
    }

    /// B1 (d): the index restored to an earlier state under a running hub,
    /// its marker with it: it holds an older text of one artifact, lacks
    /// another, and holds one since deleted. The keeper finds it within its
    /// reconcile time, with nothing noted and the counts agreeing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_index_restored_to_an_earlier_state_is_put_right_within_the_reconcile_time() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            let pace = IndexPace {
                reconcile: std::time::Duration::from_millis(300),
                ..QUICK
            };
            store.keep_index_complete(pace);
            until("filled", || index.is_complete()).await;
            until_none_pending(&store).await;

            {
                let mut held = index.held.lock().unwrap();
                held.remove("a001");
            }
            index.hold("a000", "as it was last week");
            index.hold("gone", "deleted since");
            assert!(index.is_complete() && pending(&store).await.is_empty());
            assert_eq!(index.count().await.unwrap(), 3, "as many as are stored");

            let restored = std::time::Instant::now();
            until("put right", || index.content("gone").is_none()).await;
            until("marked again", || index.is_complete()).await;
            assert!(agrees(&store, &index).await);
            assert!(restored.elapsed() < std::time::Duration::from_secs(5));
        })
        .await;
    }

    /// An index that does not hold as many artifacts as are stored, with
    /// nothing pending to explain it, is compared with Postgres by the
    /// round that finds it so, whenever the last comparison was.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_index_holding_another_number_of_artifacts_is_compared_at_once() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            store.backfill(false, |_, _| {}).await.unwrap();
            assert!(store.index_round(index.as_ref()).await.unwrap());
            index.held.lock().unwrap().remove("a001");
            assert!(index.is_complete() && pending(&store).await.is_empty());
            index.asked.lock().unwrap().clear();
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert_eq!(index.reindexed(), ["a001"]);
            assert!(agrees(&store, &index).await && index.is_complete());
        })
        .await;
    }

    /// An index that keeps no hashes is compared by id alone: what it
    /// holds of a stored artifact is taken to be right.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_index_that_keeps_no_hashes_is_compared_by_id() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            index.hashless.store(true, Ordering::SeqCst);
            store.backfill(false, |_, _| {}).await.unwrap();
            index.hold("a000", "another text");
            index.hold("gone", "deleted since");
            index.held.lock().unwrap().remove("a001");
            assert_eq!(store.differing(index.as_ref()).await.unwrap(), ["a001"]);
            assert_eq!(store.strays(index.as_ref()).await.unwrap(), ["gone"]);

            // Nothing suggesting it is wrong, it is not gone through from
            // time to time: a count says as much.
            index.asked.lock().unwrap().clear();
            index.listings.store(0, Ordering::SeqCst);
            let often = IndexPace {
                reconcile: std::time::Duration::from_millis(1),
                watch: std::time::Duration::from_millis(30),
                ..QUICK
            };
            store.backfill(false, |_, _| {}).await.unwrap();
            index.asked.lock().unwrap().clear();
            index.listings.store(0, Ordering::SeqCst);
            store.keep_index_complete(often);
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            assert!(index.asked().is_empty(), "{:?}", index.asked());
            assert_eq!(index.listings.load(Ordering::SeqCst), 0);
        })
        .await;
    }

    /// Comparing an index that agrees with Postgres reads none of what it
    /// holds, and lists its ids only when it holds more than is stored.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_index_that_agrees_is_compared_without_reading_it() {
        on_indexed_store(|store, index| async move {
            stored(&store, 250).await;
            store.backfill(false, |_, _| {}).await.unwrap();
            index.pages_read.store(0, Ordering::SeqCst);
            assert_eq!(
                store.backfill(false, |_, _| {}).await.unwrap(),
                Backfilled::default()
            );
            assert_eq!(index.pages_read.load(Ordering::SeqCst), 0, "no text read");
            assert_eq!(index.listings.load(Ordering::SeqCst), 0, "no ids listed");

            // One changed: the one page it is on is read.
            let conn = store.pool.get().await.unwrap();
            unnoted(
                &store,
                &["UPDATE artifacts SET content = 'changed' WHERE id = 'a123'"],
            )
            .await;
            let done = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!((done.indexed, done.total), (1, 1));
            assert_eq!(index.pages_read.load(Ordering::SeqCst), 1);
            assert_eq!(index.listings.load(Ordering::SeqCst), 0);

            // More held than stored: the ids are listed, once through.
            unnoted(
                &store,
                &["DELETE FROM artifacts WHERE id IN ('a001', 'a002')"],
            )
            .await;
            let done = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!((done.removed, done.total), (2, 2));
            assert_eq!(
                index.listings.load(Ordering::SeqCst),
                1,
                "by its cursor, not from the start each page"
            );
            assert!(agrees(&store, &index).await);
            drop(conn);
        })
        .await;
    }

    /// One missing and one stray: the counts agree, the stray is hidden
    /// until the missing one is in, and the same repair then finds it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stray_hidden_by_a_missing_artifact_is_found_by_the_same_repair() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            store.backfill(false, |_, _| {}).await.unwrap();
            index.held.lock().unwrap().remove("a001");
            index.hold("gone", "deleted since");
            let done = store.backfill(false, |_, _| {}).await.unwrap();
            assert_eq!((done.indexed, done.removed), (1, 1));
            assert!(agrees(&store, &index).await && index.is_complete());
        })
        .await;
    }

    /// An index that holds more artifacts than are stored is repaired by
    /// the round that counts them, with nothing else to say it is wrong.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_index_holding_more_than_is_stored_is_repaired_on_the_count() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            store.backfill(false, |_, _| {}).await.unwrap();
            assert!(store.index_round(index.as_ref()).await.unwrap());
            index.hold("gone", "deleted since");
            assert_eq!(index.ids_held().len(), 4);
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert_eq!(index.ids_held(), ["a000", "a001", "a002"]);
            assert!(agrees(&store, &index).await);
        })
        .await;
    }

    /// A backlog larger than one read of the pending list is put right in
    /// one round, for as long as it gets somewhere.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_large_backlog_is_drained_in_one_round() {
        on_indexed_store(|store, index| async move {
            index.mark_complete().await.unwrap();
            stored(&store, 1200).await;
            assert_eq!(pending(&store).await.len(), 1200);
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert!(pending(&store).await.is_empty());
            assert_eq!(index.ids_held().len(), 1200);
        })
        .await;
    }

    /// What a search waits its deadline for is the index, and not
    /// Postgres: when an index that holds nothing has answered, how many
    /// artifacts are stored is read outside the deadline. With the table
    /// held for longer than the deadline, the search is not one the index
    /// did not answer in time, and the index is not left alone for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_count_of_what_is_stored_is_not_an_index_out_of_time() {
        workforce_postgres::test_support::with_test_pool(
            KnowledgeStore::build_pool,
            |pool| async move {
                let index = Held::on(&pool);
                let deadline = std::time::Duration::from_millis(200);
                let rest = std::time::Duration::from_secs(60);
                let asked = Some(index.clone() as Arc<dyn ArtifactIndexer>);
                let store = KnowledgeStore::waiting(pool.clone(), asked, deadline, rest);
                let store = store.await.unwrap();
                stored(&store, 3).await;
                store.backfill(false, |_, _| {}).await.unwrap();
                // Emptied behind the hub, its mark left.
                index.held.lock().unwrap().clear();
                assert!(index.is_complete());

                let holding = pool.get().await.unwrap();
                holding
                    .batch_execute("BEGIN; LOCK TABLE artifacts IN ACCESS EXCLUSIVE MODE")
                    .await
                    .unwrap();
                let held_for = deadline * 4;
                let letting_go = tokio::spawn(async move {
                    tokio::time::sleep(held_for).await;
                    holding.batch_execute("COMMIT").await.unwrap();
                });
                for _ in 0..SEARCH_TRIPS_AFTER + 1 {
                    // The first of them waits for the table, far past the
                    // deadline.
                    let caller = Caller::user("bob");
                    let filter = ArtifactFilter::default();
                    let found = store.search_told("databases", &caller, &filter, 5);
                    let found = tokio::time::timeout(std::time::Duration::from_secs(20), found)
                        .await
                        .expect("answered once the table is let go")
                        .unwrap();
                    assert_eq!(found.hits.len(), 3, "by keyword");
                    assert_eq!(
                        found.by_keyword,
                        Some(Skipped::OutOfStep {
                            held: 0,
                            stored: 3,
                            pending: 0
                        })
                    );
                    assert_eq!(store.index_state().search_breaker, Some("closed"));
                }
                letting_go.await.unwrap();
            },
        )
        .await;
    }

    /// A search given a deadline waits only so long for an index that
    /// takes the question and does not answer, then goes by keyword, and
    /// says so. One slow answer costs the search that waited for it; after
    /// three in a row the index is left alone, and searches do not wait at
    /// all, wake nobody and change nothing known of the index. Its rest
    /// over, one search at a time tries it, and when it answers it is
    /// relied on again. A store given no deadline waits for its index.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_does_not_wait_on_an_index_that_never_answers() {
        workforce_postgres::test_support::with_test_pool(
            KnowledgeStore::build_pool,
            |pool| async move {
                let index = Held::on(&pool);
                let (deadline, rest) = (
                    std::time::Duration::from_millis(250),
                    std::time::Duration::from_millis(900),
                );
                let store = KnowledgeStore::waiting(
                    pool.clone(),
                    Some(index.clone() as Arc<dyn ArtifactIndexer>),
                    deadline,
                    rest,
                )
                .await
                .unwrap();
                stored(&store, 3).await;
                store.backfill(false, |_, _| {}).await.unwrap();
                let search = || async {
                    let began = std::time::Instant::now();
                    let caller = Caller::user("bob");
                    let filter = ArtifactFilter::default();
                    let found = store.search_told("databases", &caller, &filter, 5);
                    let found = tokio::time::timeout(std::time::Duration::from_secs(10), found)
                        .await
                        .expect("answered, though the index never is")
                        .unwrap();
                    (found.hits.len(), found.by_keyword, began.elapsed())
                };
                let breaker = || store.index_state().search_breaker;
                assert_eq!(search().await.1, None, "by the index");
                assert_eq!(breaker(), Some("closed"));

                index.hung.store(true, Ordering::SeqCst);
                for nth in 1..=SEARCH_TRIPS_AFTER {
                    let (found, by, took) = search().await;
                    assert_eq!(found, 3, "by keyword");
                    assert_eq!(by, Some(Skipped::TimedOut(deadline)));
                    assert!(took >= deadline && took < deadline * 3, "{took:?}");
                    let open = nth == SEARCH_TRIPS_AFTER;
                    assert_eq!(breaker(), Some(if open { "open" } else { "closed" }));
                }
                // Left alone: not asked, not waited for, and nothing a
                // search that asked nothing could have found out is acted
                // on.
                let asked = index.searches.load(Ordering::SeqCst);
                store.index.update(|state| state.complete = Some(true));
                let woken = || async {
                    let waited = std::time::Duration::from_millis(20);
                    tokio::time::timeout(waited, store.index.wake.notified())
                        .await
                        .is_ok()
                };
                // Whatever woke the keeper before is used up: a wake
                // that never stops coming is not waited out.
                let mut wakes = 0;
                while woken().await {
                    wakes += 1;
                    assert!(wakes < 100, "the keeper is woken without end");
                }
                for _ in 0..3 {
                    let (found, by, took) = search().await;
                    assert_eq!((found, by), (3, Some(Skipped::Resting)));
                    assert!(took < deadline / 2, "not waited for: {took:?}");
                }
                assert_eq!(index.searches.load(Ordering::SeqCst), asked, "left alone");
                assert_eq!(store.index_state().complete, Some(true));
                assert!(!woken().await, "the keeper is not woken");

                // Its rest over, one search tries it while the others go
                // without; still not answering, it is left alone again.
                tokio::time::sleep(rest).await;
                assert_eq!(breaker(), Some("half_open"));
                let (probe, beside) = tokio::join!(search(), async {
                    tokio::time::sleep(deadline / 5).await;
                    search().await
                });
                assert_eq!(probe.1, Some(Skipped::TimedOut(deadline)));
                assert_eq!(beside.1, Some(Skipped::Resting));
                assert_eq!(index.searches.load(Ordering::SeqCst), asked + 1);
                assert_eq!(breaker(), Some("open"));

                tokio::time::sleep(rest).await;
                index.hung.store(false, Ordering::SeqCst);
                store.backfill(false, |_, _| {}).await.unwrap();
                let (found, by, _) = search().await;
                assert_eq!((found, by), (0, None), "asked again, and relied on");
                assert_eq!(breaker(), Some("closed"));

                // With no deadline, as for an index that had none before:
                // the search waits for its index.
                let patient = KnowledgeStore::new(
                    pool.clone(),
                    Some(index.clone() as Arc<dyn ArtifactIndexer>),
                )
                .await
                .unwrap();
                assert_eq!(patient.index_state().search_breaker, None);
                index.hung.store(true, Ordering::SeqCst);
                let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                let waiting = patient.search_told("databases", &caller, &filter, 5);
                // Longer than any deadline the store would have had.
                let waited = SEARCH_DEADLINE + std::time::Duration::from_millis(500);
                assert!(tokio::time::timeout(waited, waiting).await.is_err());
            },
        )
        .await;
    }

    /// A write, or a delete, the index did not take stays noted and is put
    /// right by the next round, without a repair.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_the_index_did_not_take_is_put_right_later() {
        on_indexed_store(|store, index| async move {
            let alice = Caller::user("alice");
            index.mark_complete().await.unwrap();
            store
                .create(&artifact("kept", "alice", Visibility::Public))
                .await
                .unwrap();
            until("indexed", || index.content("kept").is_some()).await;
            until_none_pending(&store).await;

            index.down.store(true, Ordering::SeqCst);
            store
                .create(&artifact("new", "alice", Visibility::Public))
                .await
                .unwrap();
            store
                .update("kept", "changed", "alice", &alice)
                .await
                .unwrap();
            assert_eq!(pending(&store).await, ["kept", "new"]);
            assert!(
                !store.index_round(index.as_ref()).await.unwrap(),
                "still down"
            );
            assert_eq!(pending(&store).await, ["kept", "new"]);
            let said = store.index_state().last_error.unwrap();
            assert!(
                said.contains("took none of the 2") && said.contains("is down"),
                "{said}"
            );

            index.down.store(false, Ordering::SeqCst);
            index.asked.lock().unwrap().clear();
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert_eq!(index.content("kept").as_deref(), Some("changed"));
            assert!(index.content("new").is_some());
            assert!(pending(&store).await.is_empty());
            assert!(
                !index.asked().contains(&"mark incomplete".to_string()),
                "no repair for what was noted"
            );
            assert_eq!(store.index_state().pending, 0);

            // A delete the index did not take.
            index.down.store(true, Ordering::SeqCst);
            store.delete("new", &alice).await.unwrap();
            assert_eq!(pending(&store).await, ["new"]);
            index.down.store(false, Ordering::SeqCst);
            assert!(store.index_round(index.as_ref()).await.unwrap());
            assert_eq!(index.content("new"), None);
            assert!(pending(&store).await.is_empty());
        })
        .await;
    }

    /// An artifact is taken off the pending list only by a try that read
    /// the noting there: one noted again meanwhile stays, though what the
    /// try wrote in the end is the newer text, for the try did not read
    /// that noting and cannot answer for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_note_renewed_while_its_artifact_was_indexed_survives() {
        on_indexed_store(|store, index| async move {
            stored(&store, 2).await;
            let conn = store.pool.get().await.unwrap();
            let pace = IndexPace::DEFAULT;
            store.try_one(index.as_ref(), "a001", &pace).await.unwrap();
            // A write of a000 is noted while the index is taking it.
            index.keeping_back.store(true, Ordering::SeqCst);
            let (keeper, asked) = (Arc::clone(&store), Arc::clone(&index));
            let going =
                tokio::spawn(async move { keeper.try_one(asked.as_ref(), "a000", &pace).await });
            index.until_kept_back(1).await;
            conn.execute(
                "UPDATE artifacts SET title = 'Another' WHERE id = 'a000'",
                &[],
            )
            .await
            .unwrap();
            index.land(0);
            index.answer(0, true);
            // What is stored is read again, and that written too.
            index.until_kept_back(2).await;
            index.land(1);
            index.answer(1, true);
            going.await.unwrap().unwrap();
            assert_eq!(index.ids_held(), ["a000", "a001"]);
            assert_eq!(
                pending(&store).await,
                ["a000"],
                "the later write is still noted"
            );
        })
        .await;
    }

    /// The tags a tagger adds are written back to the artifact, and it is
    /// indexed with them.
    #[tokio::test(flavor = "multi_thread")]
    async fn tags_the_tagger_adds_are_written_back_and_indexed() {
        on_indexed_store(|store, index| async move {
            index.mark_complete().await.unwrap();
            *index.tagging.lock().unwrap() = Some("Runbook");
            store
                .create(&artifact("tagged", "alice", Visibility::Public))
                .await
                .unwrap();
            until("indexed", || index.content("tagged").is_some()).await;
            until_none_pending(&store).await;
            let tagged = store
                .read("tagged", &Caller::user("alice"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(tagged.tags, ["rust", "db", "runbook"]);
            let held = index.held.lock().unwrap().get("tagged").cloned().unwrap();
            assert_eq!(held.tags, tagged.tags);
            assert_eq!(held.hash, store.stored_hash("tagged").await.unwrap());
        })
        .await;
    }

    /// A search the index does not answer goes by keyword at once, as does
    /// one that finds the index emptied behind the hub, which the keeper is
    /// then woken to fill.
    ///
    /// The keeper's rounds are run here, one by one, rather than in the
    /// background: that the search wakes the keeper is seen in the wake it
    /// leaves, and that the round it wakes fills the index in that round.
    /// Nothing waits on the time a loaded machine takes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_the_index_cannot_serve_goes_by_keyword_at_once() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            // The keeper's first round compares, and fills it.
            store.round(index.as_ref(), &QUICK).await.unwrap();
            assert!(index.is_complete());
            assert_eq!(index.ids_held().len(), 3, "filled");
            let search = || async {
                let filter = ArtifactFilter::default();
                store
                    .search("databases", &Caller::user("bob"), &filter, 5)
                    .await
                    .expect("never an error for want of the index")
                    .len()
            };
            assert_eq!(
                search().await,
                0,
                "the index is relied on, and finds nothing"
            );

            index.deaf.store(true, Ordering::SeqCst);
            assert_eq!(search().await, 3, "by keyword when the search fails");
            index.deaf.store(false, Ordering::SeqCst);
            index.down.store(true, Ordering::SeqCst);
            assert_eq!(search().await, 3, "by keyword when nothing answers");
            index.down.store(false, Ordering::SeqCst);

            // Emptied, not dropped: still marked complete.
            woken(&store).await;
            index.held.lock().unwrap().clear();
            assert!(index.is_complete());
            assert_eq!(search().await, 3, "by keyword, the index holding nothing");
            assert!(woken(&store).await, "the keeper is not woken to fill it");
            store.round(index.as_ref(), &QUICK).await.unwrap();
            assert_eq!(
                index.ids_held().len(),
                3,
                "not filled again by the round it woke"
            );
            assert_eq!(search().await, 0, "the index relied on again");
        })
        .await;
    }

    /// Whether the keeper has been woken since this was last asked: a wake
    /// left for it is taken, as the keeper would take it.
    async fn woken(store: &KnowledgeStore) -> bool {
        let wake = store.index.wake.notified();
        tokio::time::timeout(std::time::Duration::ZERO, wake)
            .await
            .is_ok()
    }

    /// The keeper fills an incomplete index by itself, goes on trying
    /// while the index does not answer, saying so in its state, and is
    /// woken by a search that finds a complete index incomplete again.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_is_kept_complete_in_the_background() {
        on_indexed_store(|store, index| async move {
            stored(&store, 3).await;
            index.down.store(true, Ordering::SeqCst);
            store.keep_index_complete(QUICK);
            until("the failure is reported", || {
                store.index_state().last_error.is_some()
            })
            .await;
            let state = store.index_state();
            assert!(state.configured);
            assert_eq!(state.complete, None, "could not tell");
            assert!(
                state.last_error.as_deref().unwrap().contains("is down"),
                "{state:?}"
            );

            index.down.store(false, Ordering::SeqCst);
            until("filled once the index answers", || index.is_complete()).await;
            assert_eq!(index.ids_held().len(), 3);
            until("reported complete", || {
                let state = store.index_state();
                state.complete == Some(true) && state.last_error.is_none()
            })
            .await;

            // Lost behind the store's back: the hour's watch is not waited
            // out, because the search that finds it so wakes the keeper.
            index.held.lock().unwrap().clear();
            index.complete.store(false, Ordering::SeqCst);
            let found = store
                .search(
                    "databases",
                    &Caller::user("bob"),
                    &ArtifactFilter::default(),
                    5,
                )
                .await
                .unwrap();
            assert_eq!(found.len(), 3, "by keyword meanwhile");
            until("filled again", || {
                index.is_complete() && index.ids_held().len() == 3
            })
            .await;
        })
        .await;
    }

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(Arc<KnowledgeStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            KnowledgeStore::build_pool,
            |pool| async move {
                let store = KnowledgeStore::new(pool, None).await.expect("store");
                test(store).await;
            },
        )
        .await;
    }

    /// A `file` and a `postgres` test per conformance case.
    macro_rules! on_both_backends {
        ($($case:ident),* $(,)?) => {$(
            mod $case {
                use super::*;

                #[tokio::test]
                async fn file() {
                    let tmp = tempfile::tempdir().unwrap();
                    let backend = FsArtifactBackend::create_at(tmp.path()).unwrap();
                    conformance::$case(&backend).await;
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn postgres() {
                    with_store(|store| async move { conformance::$case(store.as_ref()).await })
                        .await;
                }
            }
        )*};
    }

    on_both_backends!(
        owner_reads_own_private_others_cannot,
        public_readable_by_all_list_respects_acl,
        search_respects_acl,
        only_the_owner_changes_an_artifact,
        a_grant_lets_its_user_read_and_nothing_more,
        grants_go_with_their_artifact,
        an_artifact_changes_owner_and_its_grants_follow,
    );

    // ===== DW-620: what the keeper of the index is held to =====
    //
    // The invariants of the keeper's redesign
    // (dw-620-artifact-keeper-design.md, section 5), written before the
    // keeper they describe. One marked `should_panic` names the invariant
    // the present keeper breaks; the commit that makes it hold takes the
    // mark away, and one that came to hold unnoticed fails as it stands.

    mod keeper {
        use super::*;
        use std::time::Duration;

        /// What time it is, as the test says.
        #[derive(Clone)]
        struct Hands(Arc<std::sync::Mutex<DateTime<Utc>>>);

        impl Hands {
            fn now(&self) -> DateTime<Utc> {
                *self.0.lock().unwrap()
            }
            fn advance(&self, by: Duration) {
                *self.0.lock().unwrap() += by;
            }
            /// On to `to`, never back.
            fn on_to(&self, to: DateTime<Utc>) {
                let mut now = self.0.lock().unwrap();
                *now = (*now).max(to);
            }
            /// As the keeper would: at once, after its wait, or at the time.
            fn follow(&self, next: Next) {
                match next {
                    Next::AtOnce => {}
                    Next::After(wait) => self.advance(wait),
                    Next::At(then) => self.on_to(then),
                }
            }
        }

        const PACE: IndexPace = IndexPace::DEFAULT;

        /// The longest a failed artifact waits to be due again.
        fn cap() -> Duration {
            PACE.backoff_cap.mul_f64(1.2)
        }

        /// A store and its index, and another store on the same database
        /// and index, both at the test's time.
        async fn timed<F, Fut>(test: F)
        where
            F: FnOnce(Arc<KnowledgeStore>, Arc<KnowledgeStore>, Arc<Held>, Hands) -> Fut,
            Fut: std::future::Future<Output = ()>,
        {
            workforce_postgres::test_support::with_test_pool(
                KnowledgeStore::build_pool,
                |pool| async move {
                    let index = Held::on(&pool);
                    // To the microsecond, as Postgres keeps a time: a time
                    // read back is then the one written, on a system whose
                    // clock gives nanoseconds as on one whose does not.
                    let now = chrono::SubsecRound::trunc_subsecs(Utc::now(), 6);
                    let hands = Hands(Arc::new(std::sync::Mutex::new(now)));
                    let mut stores = Vec::new();
                    for _ in 0..2 {
                        let (at, asked) = (hands.clone(), Arc::clone(&index));
                        let clock: Clock = Arc::new(move || at.now());
                        let asked = Some(asked as Arc<dyn ArtifactIndexer>);
                        let store = KnowledgeStore::made(pool.clone(), asked, None, clock);
                        stores.push(store.await.unwrap());
                    }
                    let other = stores.pop().unwrap();
                    test(stores.pop().unwrap(), other, index, hands).await;
                },
            )
            .await;
        }

        /// A round, which is never an error for what the index did.
        async fn round(invariant: u8, store: &KnowledgeStore, index: &Arc<Held>) -> Next {
            round_at(invariant, store, index, &PACE).await
        }

        async fn round_at(
            invariant: u8,
            store: &KnowledgeStore,
            index: &Arc<Held>,
            pace: &IndexPace,
        ) -> Next {
            let ended = store.round(index.as_ref(), pace).await;
            ended.unwrap_or_else(|e| panic!("DW-620 I{invariant}: a round was an error: {e:#}"))
        }

        macro_rules! holds {
            ($invariant:literal, $held:expr, $($why:tt)+) => {
                assert!($held, "DW-620 I{}: {}", $invariant, format!($($why)+))
            };
        }

        #[derive(Debug)]
        struct Row {
            seq: i64,
            tries: i32,
            next_try_at: Option<DateTime<Utc>>,
        }

        /// As a store whose first round has run its compare at start, and
        /// well: what its later rounds do is the work, and no compare.
        fn compared_already(store: &KnowledgeStore) {
            store.ended((store.clock)(), true);
        }

        async fn row(store: &KnowledgeStore, id: &str) -> Option<Row> {
            let conn = store.pool.get().await.unwrap();
            let read = "SELECT noted_seq, tries, next_try_at FROM artifact_index_pending \
                        WHERE artifact_id = $1";
            let row = conn.query_opt(read, &[&id]).await.unwrap()?;
            Some(Row {
                seq: row.get(0),
                tries: row.get(1),
                next_try_at: row.get(2),
            })
        }

        async fn sql(store: &KnowledgeStore, statement: &str) {
            let conn = store.pool.get().await.unwrap();
            conn.batch_execute(statement).await.unwrap();
        }

        async fn rewrite(store: &KnowledgeStore, id: &str, content: &str) {
            let conn = store.pool.get().await.unwrap();
            let change = "UPDATE artifacts SET content = $2 WHERE id = $1";
            assert_eq!(conn.execute(change, &[&id, &content]).await.unwrap(), 1);
        }

        fn tried(index: &Held, id: &str) -> usize {
            index.reindexed().iter().filter(|one| *one == id).count()
        }

        /// Invariant 1. No pending artifact is ever given up: one the
        /// index fails on fifty times is tried each time it is due, and
        /// indexed the first time the index takes it.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_artifact_the_index_fails_on_fifty_times_is_indexed_on_the_next() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 3).await;
                index.failing.lock().unwrap().insert("a001".into(), 50);
                for nth in 1..=60 {
                    round(1, &store, &index).await;
                    if index.ids_held().len() == 3 {
                        break;
                    }
                    let waiting = row(&store, "a001").await;
                    holds!(1, waiting.is_some(), "its note is gone after {nth} rounds");
                    let due = waiting.unwrap().next_try_at;
                    let in_time = due.is_none_or(|at| at <= hands.now() + cap());
                    holds!(1, in_time, "after {nth} tries it is not due until {due:?}");
                    hands.advance(cap() + PACE.retry);
                }
                holds!(
                    1,
                    index.ids_held().len() == 3,
                    "held: {:?}",
                    index.ids_held()
                );
                let tries = tried(&index, "a001");
                holds!(
                    1,
                    tries == 51,
                    "tried {tries} times, not each time it was due"
                );
            })
            .await;
        }

        /// Invariant 1, the first experiment of the eighth review: the
        /// first two writes of every round fail, whatever they are. While
        /// that lasts the two are not indexed; when it ends they are,
        /// within the wait they had come to.
        #[tokio::test(flavor = "multi_thread")]
        async fn two_writes_failing_at_the_start_of_every_round_cost_no_artifact() {
            timed(|store, _, index, hands| async move {
                stored(&store, 5).await;
                hands.follow(round(1, &store, &index).await);
                holds!(1, index.ids_held().len() == 5, "indexed to begin with");
                rewrite(&store, "a000", "said again").await;
                rewrite(&store, "a001", "said again").await;

                let in_round = Arc::new(AtomicUsize::new(0));
                let nth = Arc::clone(&in_round);
                let first_two = move |_: &str, _: bool| nth.fetch_add(1, Ordering::SeqCst) < 2;
                *index.script.lock().unwrap() = Some(Box::new(first_two));
                for _ in 0..6 {
                    in_round.store(0, Ordering::SeqCst);
                    let next = round(1, &store, &index).await;
                    hands.follow(next);
                    if matches!(next, Next::At(_)) {
                        hands.advance(Duration::from_secs(1));
                    }
                }
                *index.script.lock().unwrap() = None;
                for id in ["a000", "a001"] {
                    let waiting = row(&store, id).await;
                    holds!(1, waiting.is_some(), "the note of '{id}' is gone");
                }
                hands.advance(cap() + PACE.retry);
                hands.follow(round(1, &store, &index).await);
                for id in ["a000", "a001"] {
                    let indexed = index.content(id);
                    holds!(
                        1,
                        indexed.as_deref() == Some("said again"),
                        "'{id}': {indexed:?}"
                    );
                }
            })
            .await;
        }

        /// Invariant 1, the second experiment: an index that takes again
        /// what it holds and nothing new. Every new artifact goes on being
        /// tried, and all are indexed once it takes them.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_that_takes_nothing_new_costs_no_artifact() {
            timed(|store, _, index, hands| async move {
                stored(&store, 3).await;
                hands.follow(round(1, &store, &index).await);
                stored(&store, 23).await;
                *index.script.lock().unwrap() = Some(Box::new(|_: &str, held: bool| !held));
                for _ in 0..12 {
                    hands.follow(round(1, &store, &index).await);
                }
                holds!(
                    1,
                    noted(&store).await.len() == 20,
                    "{:?}",
                    noted(&store).await
                );
                *index.script.lock().unwrap() = None;
                hands.advance(cap() + PACE.retry);
                for _ in 0..20 {
                    hands.follow(round(1, &store, &index).await);
                }
                holds!(
                    1,
                    index.ids_held().len() == 23,
                    "held: {:?}",
                    index.ids_held()
                );
            })
            .await;
        }

        /// Invariant 1, the third experiment: one write in two fails,
        /// whatever the artifact. Every artifact is indexed all the same.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_failing_one_write_in_two_costs_no_artifact() {
            timed(|store, _, index, hands| async move {
                stored(&store, 10).await;
                hands.follow(round(1, &store, &index).await);
                stored(&store, 120).await;
                let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
                let one_in_two = move |_: &str, _: bool| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    (seed >> 33).is_multiple_of(2)
                };
                *index.script.lock().unwrap() = Some(Box::new(one_in_two));
                for _ in 0..3000 {
                    if index.ids_held().len() == 120 {
                        break;
                    }
                    hands.follow(round(1, &store, &index).await);
                }
                holds!(
                    1,
                    index.ids_held().len() == 120,
                    "{} indexed",
                    index.ids_held().len()
                );
                holds!(
                    1,
                    noted(&store).await.is_empty(),
                    "{:?}",
                    noted(&store).await
                );
            })
            .await;
        }

        /// Invariants 1 and 2 through an outage of eight hours, with few
        /// pending and with many: the index is given three artifacts a
        /// round and no more; nothing is lost; and what was tried while it
        /// was out is tried again within the longest wait.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_outage_of_eight_hours_costs_no_artifact() {
            for pending in [10usize, 1000] {
                timed(move |store, _, index, hands| async move {
                    index.mark_complete().await.unwrap();
                    stored(&store, pending).await;
                    index.down.store(true, Ordering::SeqCst);
                    let began = hands.now();
                    while hands.now() < began + Duration::from_secs(8 * 3600) {
                        let given = index.given.load(Ordering::SeqCst);
                        let next = round(1, &store, &index).await;
                        let given = index.given.load(Ordering::SeqCst) - given;
                        holds!(
                            1,
                            given <= 3,
                            "{given} artifacts given to an index that is out"
                        );
                        holds!(
                            1,
                            next != Next::AtOnce,
                            "no wait after a round that took nothing"
                        );
                        hands.follow(next);
                    }
                    index.down.store(false, Ordering::SeqCst);
                    let back = hands.now();
                    for _ in 0..(pending / 3 + 50) {
                        let next = round(1, &store, &index).await;
                        if index.ids_held().len() == pending {
                            break;
                        }
                        hands.follow(next);
                    }
                    let held = index.ids_held().len();
                    holds!(1, held == pending, "{held} of {pending} indexed");
                    let took = hands.now() - back;
                    let longest = chrono::Duration::from_std(cap() + PACE.retry * 2).unwrap();
                    holds!(1, took <= longest, "tried again only after {took}");
                })
                .await;
            }
        }

        /// Invariant 2. A round says what the keeper does next: another at
        /// once when it indexed something and more is due; its fixed wait
        /// when the index took nothing; and otherwise when the next
        /// artifact is due, or its watch when none is.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_round_says_what_the_keeper_does_next() {
            timed(|store, _, index, hands| async move {
                compared_already(&store);
                index.mark_complete().await.unwrap();
                stored(&store, 6).await;
                for bad in ["a002", "a003", "a004"] {
                    index.broken.lock().unwrap().insert(bad.to_string());
                }
                // Two taken, three failed in a row: the round ends there,
                // with one not yet tried.
                let next = round(2, &store, &index).await;
                let given = index.given.load(Ordering::SeqCst);
                holds!(2, given == 5, "{given} given: three in a row end a round");
                holds!(
                    2,
                    next == Next::AtOnce,
                    "{next:?} with one taken and one due"
                );

                // The last one taken, and nothing due before the three are.
                let next = round(2, &store, &index).await;
                holds!(2, index.ids_held().len() == 3, "{:?}", index.ids_held());
                let mut due = Vec::new();
                for bad in ["a002", "a003", "a004"] {
                    due.push(row(&store, bad).await.unwrap().next_try_at);
                }
                let first = due.iter().flatten().min().copied();
                holds!(
                    2,
                    due.iter().all(Option::is_some),
                    "each has its time: {due:?}"
                );
                holds!(
                    2,
                    Some(next) == first.map(Next::At),
                    "{next:?}, first due {first:?}"
                );

                // The three alone, and none taken: the fixed wait.
                hands.advance(cap());
                let next = round(2, &store, &index).await;
                holds!(
                    2,
                    next == Next::After(PACE.retry),
                    "{next:?} after none was taken"
                );

                // Nothing pending at all: the watch.
                index.broken.lock().unwrap().clear();
                hands.advance(cap());
                hands.follow(round(2, &store, &index).await);
                let next = round(2, &store, &index).await;
                let watch = Next::At(hands.now() + PACE.watch);
                holds!(2, next == watch, "{next:?} with nothing pending");
            })
            .await;
        }

        /// Invariant 2. A good artifact that is due waits for each bad one
        /// due before it once, three a round, whether it has failed less
        /// often than they, as often, or more.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_good_artifact_waits_for_each_bad_one_once_whatever_its_tries() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                for bad in [3usize, 30, 300] {
                    for good_tries in [1, 4, 9] {
                        sql(
                            &store,
                            "DELETE FROM artifacts; DELETE FROM artifact_index_pending",
                        )
                        .await;
                        index.held.lock().unwrap().clear();
                        index.broken.lock().unwrap().clear();
                        stored(&store, bad + 1).await;
                        let good = format!("a{bad:03}");
                        for n in 0..bad {
                            index.broken.lock().unwrap().insert(format!("a{n:03}"));
                        }
                        // All due, the bad ones for longer.
                        let conn = store.pool.get().await.unwrap();
                        let (long, lately) = (
                            hands.now() - chrono::Duration::minutes(5),
                            hands.now() - chrono::Duration::seconds(1),
                        );
                        let set = "UPDATE artifact_index_pending SET tries = $2, next_try_at = $3 \
                                   WHERE (artifact_id = $1) = $4";
                        conn.execute(set, &[&good, &4i32, &long, &false])
                            .await
                            .unwrap();
                        conn.execute(set, &[&good, &good_tries, &lately, &true])
                            .await
                            .unwrap();
                        let mut rounds = 0;
                        while index.content(&good).is_none() && rounds < bad {
                            rounds += 1;
                            let next = round(2, &store, &index).await;
                            if let Next::After(wait) = next {
                                hands.advance(wait);
                            }
                        }
                        let at_most = bad.div_ceil(3) + 1;
                        holds!(
                            2,
                            index.content(&good).is_some() && rounds <= at_most,
                            "behind {bad} bad at 4 tries, a good one at {good_tries} was not \
                             indexed in {at_most} rounds ({rounds} run)"
                        );
                    }
                }
            })
            .await;
        }

        /// Invariant 2. An artifact the index has never failed to take
        /// goes before every one it has, however long those have been due:
        /// three bad ones due for minutes do not end a round before a new
        /// write is tried.
        #[tokio::test(flavor = "multi_thread")]
        async fn what_has_never_failed_is_tried_before_what_has() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 4).await;
                for bad in ["a000", "a001", "a002"] {
                    index.broken.lock().unwrap().insert(bad.to_string());
                }
                // The three have failed once and been due for minutes; the
                // fourth was noted just now and never tried.
                let conn = store.pool.get().await.unwrap();
                let long = hands.now() - chrono::Duration::minutes(5);
                let failed = "UPDATE artifact_index_pending SET tries = 1, next_try_at = $1 \
                              WHERE artifact_id <> 'a003'";
                conn.execute(failed, &[&long]).await.unwrap();
                round(2, &store, &index).await;
                let first = index.reindexed().first().cloned();
                holds!(
                    2,
                    first.as_deref() == Some("a003"),
                    "tried first: {first:?}"
                );
                holds!(
                    2,
                    index.content("a003").is_some(),
                    "the new one not indexed"
                );
            })
            .await;
        }

        /// Invariant 2, as it stands today and is to stay: a write does
        /// not cut short the keeper's wait after a round in which the
        /// index took nothing. Each write makes its own one try.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_write_does_not_cut_short_the_wait_after_a_round_that_took_nothing() {
            on_indexed_store(|store, index| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 3).await;
                index.down.store(true, Ordering::SeqCst);
                store.keep_index_complete(IndexPace {
                    retry: Duration::from_secs(3),
                    ..QUICK
                });
                until("the round that takes nothing", || {
                    index.given.load(Ordering::SeqCst) == 3
                })
                .await;
                for n in 0..5 {
                    let written = artifact(&format!("w{n}"), "alice", Visibility::Public);
                    store.create(&written).await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let given = index.given.load(Ordering::SeqCst);
                assert_eq!(given, 3 + 5, "the round's three, and one try a write");
            })
            .await;
        }

        /// Invariant 2, as it stands today and is to stay: a write whose
        /// own try fails wakes a keeper that has nothing due, so that the
        /// artifact is tried again when it is due and not at the end of
        /// the keeper's watch.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_write_the_index_did_not_take_wakes_an_idle_keeper() {
            on_indexed_store(|store, index| async move {
                index.mark_complete().await.unwrap();
                store.keep_index_complete(IndexPace {
                    retry: Duration::from_millis(50),
                    backoff: Duration::from_millis(100),
                    ..QUICK
                });
                tokio::time::sleep(Duration::from_millis(300)).await;
                index.failing.lock().unwrap().insert("late".to_string(), 1);
                let written = artifact("late", "alice", Visibility::Public);
                store.create(&written).await.unwrap();
                until("indexed long before the watch is over", || {
                    index.content("late").is_some()
                })
                .await;
            })
            .await;
        }

        /// Invariant 3. A changed artifact is due at once, and a failure
        /// of the version before does not put it off: the failure is of a
        /// noting that is no longer the one there.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_failure_of_the_older_version_does_not_put_off_the_newer() {
            timed(|store, _, index, _| async move {
                compared_already(&store);
                index.mark_complete().await.unwrap();
                stored(&store, 2).await;
                index.keeping_back.store(true, Ordering::SeqCst);
                let (keeper, asked) = (Arc::clone(&store), Arc::clone(&index));
                let going = tokio::spawn(async move { round(3, &keeper, &asked).await });
                index.until_kept_back(1).await;
                assert_eq!(index.kept_back()[0].1, "a000");
                rewrite(&store, "a000", "the newer text").await;
                let noted_again = row(&store, "a000").await.unwrap().seq;
                index.answer(0, false);
                index.until_kept_back(2).await;
                index.land(1);
                index.answer(1, true);
                let ended = tokio::time::timeout(Duration::from_secs(10), going).await;
                ended.expect("the round ends").unwrap();
                let newer = row(&store, "a000").await;
                holds!(3, newer.is_some(), "the newer noting is gone");
                let newer = newer.unwrap();
                holds!(
                    3,
                    newer.tries == 0 && newer.next_try_at.is_none() && newer.seq > noted_again,
                    "put off by a failure of the version before: {newer:?}"
                );
            })
            .await;
        }

        /// Invariant 4. A failure always leaves the artifact noted, also
        /// when its note is gone by then: another hub indexed it meanwhile
        /// and took the note off, and what this try sent may yet land over
        /// that. The note made is a new one: due at once, with no try
        /// counted against it.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_failure_leaves_a_note_though_another_took_the_note_off() {
            timed(|store, _, index, _| async move {
                compared_already(&store);
                index.mark_complete().await.unwrap();
                stored(&store, 1).await;
                let first = row(&store, "a000").await.unwrap().seq;
                index.keeping_back.store(true, Ordering::SeqCst);
                let (keeper, asked) = (Arc::clone(&store), Arc::clone(&index));
                let going = tokio::spawn(async move { round(4, &keeper, &asked).await });
                index.until_kept_back(1).await;
                // As another hub's success leaves the list.
                sql(&store, "DELETE FROM artifact_index_pending").await;
                index.answer(0, false);
                let ended = tokio::time::timeout(Duration::from_secs(10), going).await;
                ended.expect("the round ends").unwrap();
                let noted = row(&store, "a000").await;
                holds!(4, noted.is_some(), "failed, and nothing noted of it");
                let noted = noted.unwrap();
                holds!(
                    4,
                    noted.seq > first && noted.tries == 0 && noted.next_try_at.is_none(),
                    "not a new note, due at once: {noted:?}"
                );
            })
            .await;
        }

        /// Invariant 4. A success removes only the note it read. The text
        /// goes X, Y, X: one hub's write of the first X lands, another
        /// indexes Y, the text goes back to X, and only then does the
        /// first hub see its write taken. Its hash is the stored one
        /// again; the note is not its own, and stays.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_success_does_not_remove_a_note_made_after_the_write_it_read() {
            timed(|one, other, index, hands| async move {
                compared_already(&one);
                compared_already(&other);
                index.mark_complete().await.unwrap();
                stored(&one, 1).await;
                let first = one.load_hashed("a000").await.unwrap().unwrap().0.content;
                index.keeping_back.store(true, Ordering::SeqCst);

                let (keeper, asked) = (Arc::clone(&one), Arc::clone(&index));
                let slow = tokio::spawn(async move { round(4, &keeper, &asked).await });
                index.until_kept_back(1).await;
                index.land(0);

                rewrite(&one, "a000", "Y").await;
                let (keeper, asked) = (Arc::clone(&other), Arc::clone(&index));
                let quick = tokio::spawn(async move { round(4, &keeper, &asked).await });
                index.until_kept_back(2).await;
                index.land(1);
                index.answer(1, true);
                let ended = tokio::time::timeout(Duration::from_secs(10), quick).await;
                ended.expect("the other hub's round ends").unwrap();
                assert_eq!(index.content("a000").as_deref(), Some("Y"));

                rewrite(&one, "a000", &first).await;
                index.answer(0, true);
                let ended = tokio::time::timeout(Duration::from_secs(10), slow).await;
                ended.expect("the first hub's round ends").unwrap();
                // The index holds Y, and X is stored.
                let waiting = row(&one, "a000").await;
                holds!(
                    4,
                    waiting.is_some(),
                    "the index is behind what is stored, with no note"
                );

                index.keeping_back.store(false, Ordering::SeqCst);
                hands.advance(cap());
                round(4, &one, &index).await;
                let indexed = index.content("a000");
                holds!(4, indexed.as_deref() == Some(first.as_str()), "{indexed:?}");
            })
            .await;
        }

        /// Invariant 4, as it stands today and is to stay: a write that
        /// turns out older than what is stored is not a success. A slow
        /// write of X lands after a quicker one of the newer Y; whoever
        /// sent X reads what is stored again, finds Y, and writes Y.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_write_found_older_than_what_is_stored_is_written_again() {
            timed(|store, _, index, _| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 1).await;
                index.keeping_back.store(true, Ordering::SeqCst);
                let (keeper, asked) = (Arc::clone(&store), Arc::clone(&index));
                let slow = tokio::spawn(async move { keeper.round(asked.as_ref(), &PACE).await });
                index.until_kept_back(1).await;

                rewrite(&store, "a000", "Y").await;
                let (keeper, asked) = (Arc::clone(&store), Arc::clone(&index));
                let quick = tokio::spawn(async move { keeper.round(asked.as_ref(), &PACE).await });
                index.until_kept_back(2).await;
                index.land(1);
                index.answer(1, true);
                let ended = tokio::time::timeout(Duration::from_secs(10), quick).await;
                ended.expect("the quicker round ends").unwrap().unwrap();

                // The write of X lands last, and is then answered.
                index.land(0);
                assert_ne!(index.content("a000").as_deref(), Some("Y"));
                index.answer(0, true);
                index.until_kept_back(3).await;
                assert_eq!(index.kept_back()[2].1, "a000", "written again");
                index.land(2);
                index.answer(2, true);
                let ended = tokio::time::timeout(Duration::from_secs(10), slow).await;
                ended.expect("the slower round ends").unwrap().unwrap();
                assert_eq!(index.content("a000").as_deref(), Some("Y"));
            })
            .await;
        }

        /// Invariant 5. Two keepers at once lose nothing: both work the
        /// same rows, one of them an artifact the index takes only at the
        /// seventh try, and every artifact ends indexed as it is stored
        /// with nothing left noted.
        #[tokio::test(flavor = "multi_thread")]
        async fn two_keepers_at_once_lose_nothing() {
            timed(|one, other, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&one, 30).await;
                index.failing.lock().unwrap().insert("a007".into(), 6);
                for _ in 0..200 {
                    if index.ids_held().len() == 30 && noted(&one).await.is_empty() {
                        break;
                    }
                    let (first, second) =
                        tokio::join!(round(5, &one, &index), round(5, &other, &index));
                    match (first, second) {
                        (Next::AtOnce, _) | (_, Next::AtOnce) => {}
                        (Next::After(wait), _) | (_, Next::After(wait)) => hands.advance(wait),
                        (Next::At(a), Next::At(b)) => hands.on_to(a.min(b)),
                    }
                }
                let held = index.ids_held().len();
                holds!(
                    5,
                    held == 30,
                    "{held} of 30 indexed: {:?}",
                    noted(&one).await
                );
                holds!(5, noted(&one).await.is_empty(), "{:?}", noted(&one).await);
            })
            .await;
        }

        /// Invariant 6. The count is checked with pending rows present:
        /// with one artifact the index never takes, an index emptied
        /// behind the hub is found out and filled again.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_emptied_index_is_found_out_though_an_artifact_is_always_failing() {
            timed(|store, _, index, hands| async move {
                stored(&store, 21).await;
                index.broken.lock().unwrap().insert("a020".to_string());
                index.mark_complete().await.unwrap();
                hands.follow(round(6, &store, &index).await);
                holds!(6, index.ids_held().len() == 20, "{:?}", index.ids_held());
                // Emptied behind the hub, its mark left.
                index.held.lock().unwrap().clear();
                for _ in 0..3 {
                    let next = round(6, &store, &index).await;
                    if let Next::After(wait) = next {
                        hands.advance(wait);
                    }
                }
                let held = index.ids_held().len();
                holds!(
                    6,
                    held == 20,
                    "{held} of 20 indexed again after three rounds"
                );
            })
            .await;
        }

        /// Invariant 6, the band itself: an index in step holds between
        /// what is stored less what is pending and what is stored plus it.
        #[test]
        fn an_index_in_step_holds_within_what_is_pending_of_what_is_stored() {
            assert!(in_step(300, 300, 0));
            assert!(!in_step(299, 300, 0) && !in_step(301, 300, 0));
            assert!(in_step(292, 300, 8) && in_step(308, 300, 8));
            assert!(!in_step(291, 300, 8) && !in_step(309, 300, 8));
            assert!(in_step(0, 10, 10), "all pending, nothing held");
            assert!(!in_step(5, 300, 8), "made again behind the hub");
            assert!(in_step(0, 0, 0));
        }

        /// Invariant 6. One count out by one starts no compare: Postgres
        /// and the index are read at two moments, and a write between them
        /// is no fault. Out twice running, the index is compared.
        #[tokio::test(flavor = "multi_thread")]
        async fn one_count_out_by_one_starts_no_compare() {
            timed(|store, _, index, hands| async move {
                stored(&store, 5).await;
                hands.follow(round(6, &store, &index).await);
                index.mark_complete().await.unwrap();
                round(6, &store, &index).await;
                let compared = index.compared.load(Ordering::SeqCst);
                index.miscounts.store(1, Ordering::SeqCst);
                round(6, &store, &index).await;
                round(6, &store, &index).await;
                let after = index.compared.load(Ordering::SeqCst);
                holds!(6, after == compared, "compared on one count out by one");
                index.miscounts.store(usize::MAX, Ordering::SeqCst);
                round(6, &store, &index).await;
                round(6, &store, &index).await;
                let after = index.compared.load(Ordering::SeqCst);
                holds!(6, after > compared, "not compared though out twice running");
            })
            .await;
        }

        /// Invariant 6, on a search: an index that holds fewer artifacts
        /// than are stored, with nothing pending to explain it, is not
        /// searched.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_out_of_step_with_what_is_stored_is_not_searched() {
            timed(|store, _, index, hands| async move {
                stored(&store, 20).await;
                hands.follow(round(6, &store, &index).await);
                index.mark_complete().await.unwrap();
                let search = || async {
                    let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                    let found = store.search_told("databases", &caller, &filter, 5);
                    found.await.unwrap()
                };
                assert_eq!(
                    search().await.by_keyword,
                    None,
                    "by the index while in step"
                );
                for n in 0..5 {
                    index.held.lock().unwrap().remove(&format!("a{n:03}"));
                }
                let found = search().await;
                holds!(
                    6,
                    found.by_keyword.is_some(),
                    "searched 15 of 20 by the index"
                );
            })
            .await;
        }

        /// Invariant 7. A search goes by the index while what is pending
        /// is within `max(10, 1%)` of what is stored, and then says how
        /// many artifacts the index could not have found; beyond that, and
        /// with no marker, it goes by keyword.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_search_by_the_index_says_how_many_artifacts_are_not_indexed() {
            timed(|store, _, index, _| async move {
                stored(&store, 1).await;
                let search = || async {
                    let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                    let found = store.search_told("databases", &caller, &filter, 5);
                    found.await.unwrap()
                };
                for (stored, pending, by_index) in [
                    (500usize, 10usize, true),
                    (500, 11, false),
                    (5000, 50, true),
                    (5000, 51, false),
                ] {
                    // `stored` artifacts, all but `pending` of them indexed.
                    sql(&store, "DELETE FROM artifacts WHERE id <> 'a000'").await;
                    let copies = |from: usize, to: usize| {
                        format!(
                            "INSERT INTO artifacts SELECT 'b' || n, artifact_type, title, content, \
                             owner_id, visibility, tags, status, version, created_by, \
                             last_modified_by, created_at, updated_at \
                             FROM artifacts, generate_series({from}, {to}) AS n WHERE id = 'a000'"
                        )
                    };
                    let indexed = stored - pending;
                    unnoted(&store, &[copies(2, indexed).as_str()]).await;
                    sql(&store, "DELETE FROM artifact_index_pending").await;
                    sql(&store, &copies(indexed + 1, stored)).await;
                    index.held.lock().unwrap().clear();
                    index.hold("a000", "held");
                    for n in 2..=indexed {
                        index.hold(&format!("b{n}"), "held");
                    }
                    index.mark_complete().await.unwrap();
                    assert_eq!(noted(&store).await.len(), pending);

                    let found = search().await;
                    let said = (found.by_keyword.is_none(), found.not_indexed);
                    let wanted = (by_index, if by_index { pending } else { 0 });
                    holds!(
                        7,
                        said == wanted,
                        "{pending} pending of {stored}: by the index {}, not indexed {}",
                        said.0,
                        said.1
                    );
                }
                index.mark_incomplete().await.unwrap();
                assert!(search().await.by_keyword.is_some(), "no marker: by keyword");
            })
            .await;
        }

        /// Invariant 8. A try at one artifact does not outlive the
        /// keeper's deadline, whatever the index's own client would wait;
        /// and cut off, it leaves the artifact noted afresh, to be tried
        /// again, for the write it sent may yet land.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_try_cut_off_by_its_deadline_ends_and_leaves_its_note() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 1).await;
                let first = row(&store, "a000").await.unwrap().seq;
                index.keeping_back.store(true, Ordering::SeqCst);
                let hasty = IndexPace {
                    deadline: Duration::from_millis(300),
                    ..PACE
                };
                let going = round_at(8, &store, &index, &hasty);
                let ended = tokio::time::timeout(Duration::from_secs(5), going).await;
                holds!(
                    8,
                    ended.is_ok(),
                    "a write never answered kept the round waiting"
                );
                let cut_off = row(&store, "a000").await;
                holds!(8, cut_off.is_some(), "cut off, and its note gone");
                let cut_off = cut_off.unwrap();
                holds!(
                    8,
                    cut_off.seq > first && cut_off.tries == 1 && cut_off.next_try_at.is_some(),
                    "cut off, and not noted afresh to be tried again: {cut_off:?}"
                );
                // The write lands after all, and after a newer one. That
                // is the residue the design accepts: nothing more is asked
                // here than that the newer one was indexed in its turn.
                rewrite(&store, "a000", "Y").await;
                index.keeping_back.store(false, Ordering::SeqCst);
                hands.advance(cap());
                round_at(8, &store, &index, &hasty).await;
                let indexed = index.content("a000");
                holds!(8, indexed.as_deref() == Some("Y"), "{indexed:?}");
            })
            .await;
        }

        /// An artifact the index did not take waits half a minute for its
        /// next try, then one, two, four and eight, and a quarter of an
        /// hour from then on for ever; each a fifth more or less, by the
        /// artifact and the try, and the same each time it is worked out.
        #[test]
        fn the_wait_before_the_next_try_doubles_to_its_cap_and_is_the_same_each_time() {
            let secs = [30u64, 60, 120, 240, 480, 900, 900, 900];
            for (nth, secs) in secs.into_iter().enumerate() {
                let tries = nth as i32 + 1;
                let mut seen = BTreeSet::new();
                for n in 0..200 {
                    let id = format!("a{n:03}");
                    let wait = backoff(&PACE, &id, tries);
                    assert_eq!(wait, backoff(&PACE, &id, tries), "the same again");
                    let (least, most) = (
                        Duration::from_secs(secs).mul_f64(0.8),
                        Duration::from_secs(secs).mul_f64(1.2),
                    );
                    assert!(wait >= least && wait <= most, "try {tries}: {wait:?}");
                    seen.insert(wait);
                }
                assert!(
                    seen.len() > 100,
                    "spread, not all due together: {}",
                    seen.len()
                );
            }
            for tries in [9, 100, i32::MAX] {
                assert!(backoff(&PACE, "a000", tries) <= cap(), "try {tries}");
            }
            assert!(backoff(&PACE, "a000", 0) <= Duration::from_secs(36));
        }

        /// What the index said of an artifact it did not take is kept with
        /// it, cut to a length a row can bear, with when it first failed
        /// and when it is next tried; and is told to whoever asks how the
        /// index is.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_artifact_the_index_did_not_take_is_named_with_why_and_when() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 2).await;
                index.broken.lock().unwrap().insert("a001".to_string());
                let began = hands.now();
                round(1, &store, &index).await;
                hands.advance(cap());
                round(1, &store, &index).await;
                let state = store.index_state();
                assert_eq!((state.pending, state.failing.len()), (1, 1));
                let named = &state.failing[0];
                assert_eq!((named.id.as_str(), named.tries), ("a001", 2));
                assert!(named.error.contains("500: internal error"), "{named:?}");
                let first = named.first_failed_at.unwrap();
                assert!(
                    (first - began).num_milliseconds().abs() < 5,
                    "when it first failed"
                );
                let next = named.next_try_at.unwrap() - hands.now();
                assert!(
                    next >= chrono::Duration::seconds(48) && next <= chrono::Duration::seconds(72)
                );
                // The last round gave the index that one alone.
                let said = state.last_error.unwrap();
                assert!(said.contains("took none of the 1"), "{said}");
            })
            .await;
        }

        /// Invariant 12, the store's half: while a write has found the
        /// index is not the one the hub made, no search is answered from
        /// it, however small the store (one of ten artifacts passes both
        /// the tolerance and the count band, holding what was written into
        /// it); it is said, and found again once a write finds it the
        /// hub's.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_found_not_ours_is_not_searched_in_a_store_of_any_size() {
            timed(|store, _, index, hands| async move {
                stored(&store, 10).await;
                hands.follow(round(9, &store, &index).await);
                let search = || async {
                    let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                    let found = store.search_told("databases", &caller, &filter, 5);
                    found.await.unwrap()
                };
                assert_eq!(search().await.by_keyword, None);
                let made = "made by Weaviate itself (its auto-schema)".to_string();
                *index.not_ours.lock().unwrap() = Some(made.clone());
                let found = search().await;
                assert_eq!(found.by_keyword, Some(Skipped::NotOurs(made.clone())));
                assert_eq!(found.hits.len(), 5, "by keyword");
                assert_eq!(store.index_state().not_ours, Some(made));
                *index.not_ours.lock().unwrap() = None;
                assert_eq!(search().await.by_keyword, None);
                assert_eq!(store.index_state().not_ours, None);
            })
            .await;
        }

        /// What the index said of an artifact it did not take is kept with
        /// it cut to what a row can bear, however much it said.
        #[tokio::test(flavor = "multi_thread")]
        async fn what_the_index_said_is_kept_cut_to_a_bearable_length() {
            timed(|store, _, index, _| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 1).await;
                index.long_winded.lock().unwrap().insert("a000".to_string());
                round(1, &store, &index).await;
                let kept = &store.index_state().failing[0].error;
                assert_eq!(kept.chars().count(), LAST_ERROR_KEPT, "{} kept", kept.len());
                assert!(kept.contains("explains at length"));
            })
            .await;
        }

        /// Invariant 6, on a search, with artifacts pending: an index holding
        /// 5 of 300 stored, with 8 pending (within the tolerance), is out of
        /// step, and is not searched. That pending rows widen the band is
        /// no licence to skip it.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_out_of_step_is_not_searched_with_artifacts_pending() {
            timed(|store, _, index, hands| async move {
                stored(&store, 300).await;
                hands.follow(round(6, &store, &index).await);
                index.mark_complete().await.unwrap();
                sql(
                    &store,
                    "UPDATE artifacts SET content = content || ' again' WHERE id < 'a008'",
                )
                .await;
                assert_eq!(noted(&store).await.len(), 8);
                {
                    let mut held = index.held.lock().unwrap();
                    let keep: Vec<String> = held.keys().take(5).cloned().collect();
                    held.retain(|id, _| keep.contains(id));
                }
                let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                let found = store
                    .search_told("databases", &caller, &filter, 5)
                    .await
                    .unwrap();
                let out = Skipped::OutOfStep {
                    held: 5,
                    stored: 300,
                    pending: 8,
                };
                assert_eq!(found.by_keyword, Some(out));
            })
            .await;
        }

        /// What health says of how a search goes is what a search does: an
        /// index whose count is out of step at the keeper's last reading,
        /// one a compare cannot explain, is said to be searched by
        /// keyword, as it is. At the tolerance's edge it says the same as
        /// a search too.
        #[tokio::test(flavor = "multi_thread")]
        async fn health_says_a_search_goes_as_a_search_does() {
            timed(|store, _, index, hands| async move {
                stored(&store, 30).await;
                hands.follow(round(7, &store, &index).await);
                index.mark_complete().await.unwrap();
                round(7, &store, &index).await;
                let search = || async {
                    let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                    let found = store.search_told("databases", &caller, &filter, 5);
                    found.await.unwrap().by_keyword
                };
                assert_eq!(search().await, None);
                assert_eq!(store.index_state().search, Some("index"));

                // A count no compare can explain.
                index.miscounts.store(usize::MAX, Ordering::SeqCst);
                for _ in 0..3 {
                    hands.follow(round(7, &store, &index).await);
                }
                assert!(search().await.is_some());
                assert_eq!(store.index_state().in_step, Some(false));
                assert_eq!(store.index_state().search, Some("keyword"));
                index.miscounts.store(0, Ordering::SeqCst);
                round(7, &store, &index).await;
                assert_eq!(store.index_state().search, Some("index"));

                // Ten pending of thirty, and eleven, the index taking none.
                index.down.store(true, Ordering::SeqCst);
                for (pending, by) in [(10usize, "index"), (11, "keyword")] {
                    sql(&store, "DELETE FROM artifact_index_pending").await;
                    let some = format!(
                        "UPDATE artifacts SET content = content || ' {pending}' \
                         WHERE id IN (SELECT id FROM artifacts ORDER BY id LIMIT {pending})"
                    );
                    sql(&store, &some).await;
                    round(7, &store, &index).await;
                    assert_eq!(
                        store.index_state().search,
                        Some(by),
                        "{pending} pending of 30"
                    );
                }
            })
            .await;
        }

        /// A search looks at the marker again after the index answers: an
        /// index lost and made again while the question was asked is not
        /// relied on for the answer.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_lost_while_it_was_searched_is_not_relied_on() {
            timed(|store, _, index, hands| async move {
                stored(&store, 5).await;
                hands.follow(round(6, &store, &index).await);
                index.mark_complete().await.unwrap();
                index.lost_while_searched.store(true, Ordering::SeqCst);
                let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                let found = store
                    .search_told("databases", &caller, &filter, 5)
                    .await
                    .unwrap();
                assert_eq!(found.by_keyword, Some(Skipped::Incomplete));
            })
            .await;
        }

        /// A compare notes what differs and leaves what is noted already as
        /// it is: an artifact failing, with its tries and its wait, is not
        /// made due at once again by a compare that finds it differs.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_compare_leaves_what_is_pending_as_it_is() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 2).await;
                index.broken.lock().unwrap().insert("a001".to_string());
                for _ in 0..3 {
                    round(1, &store, &index).await;
                    hands.advance(cap());
                }
                let before = row(&store, "a001").await.unwrap();
                assert_eq!(before.tries, 3);
                // And one it notes afresh: never failed, due now.
                index.held.lock().unwrap().remove("a000");
                assert!(row(&store, "a000").await.is_none());
                let lock = BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false);
                let lock = lock.await.unwrap();
                assert_eq!(store.compare(index.as_ref(), &lock).await.unwrap(), 2);
                lock.release().await;
                let after = row(&store, "a001").await.unwrap();
                assert_eq!(
                    (after.seq, after.tries, after.next_try_at),
                    (before.seq, before.tries, before.next_try_at)
                );
                let fresh = row(&store, "a000").await.unwrap();
                assert_eq!(
                    (fresh.tries, fresh.next_try_at),
                    (0, None),
                    "noted afresh as if it had failed"
                );
            })
            .await;
        }

        /// A hub whose lock that says it serves the index is lost with its
        /// connection takes it again, and a seeding is refused again.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_hub_takes_again_the_lock_that_says_it_serves() {
            on_indexed_store(|store, index| async move {
                stored(&store, 2).await;
                store.keep_index_complete(QUICK);
                until("filled", || index.ids_held().len() == 2).await;
                let (seeding, _) = unserved(&store).await;
                assert!(seeding.seed_index(|_, _| {}).await.is_err());
                // Its connection is lost.
                let key = serving_lock(index.backend());
                let lost = "SELECT pg_terminate_backend(pid) FROM pg_locks \
                            WHERE locktype = 'advisory' AND pid <> pg_backend_pid() \
                              AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                              AND (classid::bigint << 32) | objid::bigint = $1";
                let conn = store.pool.get().await.unwrap();
                let ended = conn.execute(lost, &[&key]).await.unwrap();
                assert_eq!(ended, 1);
                drop(conn);
                // Woken, the keeper takes it again in its next round.
                let begun = std::time::Instant::now();
                loop {
                    store.index.wake.notify_one();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if seeding.seed_index(|_, _| {}).await.is_err() {
                        break;
                    }
                    assert!(begun.elapsed() < Duration::from_secs(10), "never taken again");
                }
            })
            .await;
        }

        /// How the index stands is told as of the keeper's last round: how
        /// many are pending and failing, how many of those wait the
        /// longest between tries, since when the oldest has failed, and
        /// whether a search goes by the index.
        #[tokio::test(flavor = "multi_thread")]
        async fn how_the_index_stands_is_told_as_of_the_last_round() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 30).await;
                for bad in ["a000", "a001"] {
                    index.broken.lock().unwrap().insert(bad.to_string());
                }
                let began = hands.now();
                round(7, &store, &index).await;
                let state = store.index_state();
                assert_eq!(
                    (state.pending, state.failing.len(), state.failing_at_cap),
                    (2, 2, 0)
                );
                assert_eq!(state.oldest_failure_at, Some(began));
                assert_eq!(state.search, Some("index"), "2 of 30 is within ten");
                assert_eq!(state.counted_at, Some(began), "as of that round");
                // Failing at every try until they wait the longest.
                for _ in 0..6 {
                    hands.advance(cap());
                    round(7, &store, &index).await;
                }
                let state = store.index_state();
                assert_eq!(state.failing_at_cap, 2, "{:?}", state.failing);
                assert_eq!(state.oldest_failure_at, Some(began));
                // One that first fails later is not the oldest.
                index.broken.lock().unwrap().insert("a002".to_string());
                rewrite(&store, "a002", "said again").await;
                round(7, &store, &index).await;
                let state = store.index_state();
                assert_eq!(state.failing.len(), 3);
                assert_eq!(state.oldest_failure_at, Some(began), "the oldest still");
                // Eleven more pending, which the index does not take: by
                // keyword.
                index.down.store(true, Ordering::SeqCst);
                sql(
                    &store,
                    "UPDATE artifacts SET content = content || ' again' WHERE id > 'a018'",
                )
                .await;
                round(7, &store, &index).await;
                let state = store.index_state();
                assert_eq!(
                    state.search,
                    Some("keyword"),
                    "{} pending of 30",
                    state.pending
                );
            })
            .await;
        }

        /// The compare writes nothing to the index: it notes what differs,
        /// each stored artifact the index lacks or holds another text of
        /// and each vector whose artifact is gone, and marks an index not
        /// yet marked. Whoever works the pending list does the writing.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_compare_notes_what_differs_and_writes_nothing() {
            on_indexed_store(|store, index| async move {
                stored(&store, 4).await;
                assert!(store.index_round(index.as_ref()).await.unwrap());
                unnoted(
                    &store,
                    &[
                        "UPDATE artifacts SET content = 'changed' WHERE id = 'a001'",
                        "DELETE FROM artifacts WHERE id = 'a002'",
                    ],
                )
                .await;
                index.held.lock().unwrap().remove("a003");
                index.hold("gone", "of an artifact removed long ago");
                index.mark_incomplete().await.unwrap();
                index.asked.lock().unwrap().clear();
                assert!(noted(&store).await.is_empty());

                let lock = BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false);
                let lock = lock.await.unwrap();
                let differed = store.compare(index.as_ref(), &lock).await.unwrap();
                lock.release().await;
                assert_eq!(differed, 4);
                assert_eq!(index.asked(), ["mark incomplete", "mark complete"]);
                assert_eq!(
                    index.content("a001").as_deref(),
                    Some(artifact("a", "a", Visibility::Public).content.as_str())
                );
                let rows = noted(&store).await;
                let ids: Vec<&str> = rows.iter().map(|(id, _, _)| id.as_str()).collect();
                assert_eq!(ids, ["a001", "a002", "a003", "gone"]);
                assert!(rows.iter().all(|(_, _, tries)| *tries == 0));
                // Each under the hash of what is stored now; none for one
                // that is not stored.
                let stored_hash = store.stored_hash("a001").await.unwrap();
                assert_eq!(rows[0].1, stored_hash);
                assert_eq!((rows[1].1.as_str(), rows[3].1.as_str()), (DELETED, DELETED));

                // Worked, the index is as Postgres has it.
                assert!(store.index_round(index.as_ref()).await.unwrap());
                assert!(agrees(&store, &index).await);
            })
            .await;
        }

        /// An index is marked when it has been compared, which is before
        /// it is filled: so a search goes by the index only while what is
        /// pending is no more than it may leave out (`max(10, 1%)` of what
        /// is stored), and by keyword, saying so, beyond that.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_marked_index_is_searched_only_while_little_is_pending() {
            on_indexed_store(|store, index| async move {
                let search = || async {
                    let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                    let found = store.search_told("databases", &caller, &filter, 5);
                    found.await.unwrap()
                };
                // A new index, compared and not yet filled.
                unnoted(&store, &[]).await;
                stored(&store, 30).await;
                sql(&store, "DELETE FROM artifact_index_pending").await;
                let lock = BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false);
                let lock = lock.await.unwrap();
                assert_eq!(store.compare(index.as_ref(), &lock).await.unwrap(), 30);
                lock.release().await;
                assert!(index.is_complete(), "compared");
                let found = search().await;
                let behind = Skipped::Behind {
                    pending: 30,
                    stored: 30,
                };
                assert_eq!(found.by_keyword, Some(behind));
                assert_eq!(found.hits.len(), 5, "found all the same, by keyword");

                // Filled but for ten, and but for eleven.
                assert!(store.index_round(index.as_ref()).await.unwrap());
                assert_eq!(search().await.by_keyword, None);
                for (changed, by_index) in [(10, true), (11, false)] {
                    let some = format!(
                        "UPDATE artifacts SET content = content || ' databases again' \
                         WHERE id IN (SELECT id FROM artifacts ORDER BY id LIMIT {changed})"
                    );
                    sql(&store, "DELETE FROM artifact_index_pending").await;
                    sql(&store, &some).await;
                    assert_eq!(noted(&store).await.len(), changed);
                    let found = search().await;
                    assert_eq!(
                        found.by_keyword.is_none(),
                        by_index,
                        "{changed} of 30 pending"
                    );
                }
                assert_eq!(search_tolerance(500), 10);
                assert_eq!(search_tolerance(5000), 50);
                assert_eq!(search_tolerance(5099), 50);
            })
            .await;
        }

        /// Every row of the pending list, whole.
        async fn pending_rows(store: &KnowledgeStore) -> Vec<String> {
            let conn = store.pool.get().await.unwrap();
            let whole = "SELECT p::text FROM artifact_index_pending p ORDER BY artifact_id";
            let rows = conn.query(whole, &[]).await.unwrap();
            rows.iter().map(|row| row.get(0)).collect()
        }

        /// A second index, and the store a seeding of it runs from.
        async fn unserved(store: &KnowledgeStore) -> (Arc<KnowledgeStore>, Arc<Held>) {
            let second = Held::on(&store.pool);
            (store_on(&store.pool, &second).await, second)
        }

        /// Invariant 10. Seeding a second index asks nothing of the
        /// pending list and leaves it as it was: the list is the serving
        /// index's, with what that index has yet to take and what it is
        /// failing on. The second index is filled and marked; the serving
        /// one is searched as before.
        #[tokio::test(flavor = "multi_thread")]
        async fn seeding_a_second_index_leaves_the_pending_list_as_it_was() {
            on_indexed_store(|store, index| async move {
                stored(&store, 4).await;
                index.broken.lock().unwrap().insert("a001".to_string());
                let _ = store.index_round(index.as_ref()).await;
                let _ = store.index_round(index.as_ref()).await;
                rewrite(&store, "a002", "changed, and not yet indexed").await;
                let conn = store.pool.get().await.unwrap();
                conn.execute("DELETE FROM artifacts WHERE id = 'a003'", &[])
                    .await
                    .unwrap();
                let before = pending_rows(&store).await;
                assert_eq!(before.len(), 3, "failing, changed and removed: {before:?}");
                let search = || async {
                    let (caller, filter) = (Caller::user("bob"), ArtifactFilter::default());
                    let found = store.search_told("databases", &caller, &filter, 5);
                    found.await.unwrap().by_keyword
                };
                let searched = search().await;

                let (seeding, second) = unserved(&store).await;
                second.hold("gone-long-ago", "of an artifact no longer stored");
                let done = seeding.seed_index(|_, _| {}).await.unwrap();
                assert_eq!((done.indexed, done.removed, done.failed), (3, 1, 0));
                assert_eq!(second.ids_held(), ["a000", "a001", "a002"]);
                assert_eq!(
                    second.content("a002").as_deref(),
                    Some("changed, and not yet indexed")
                );
                assert!(second.is_complete(), "marked, having taken everything");
                assert_eq!(pending_rows(&store).await, before, "not a row touched");
                assert_eq!(
                    search().await,
                    searched,
                    "the serving index searched as before"
                );
                assert_eq!(
                    index.content("a002").as_deref(),
                    Some(artifact("a", "a", Visibility::Public).content.as_str())
                );
            })
            .await;
        }

        /// Invariant 10. A seeding stops after three artifacts in a row
        /// that its index will not take, says which, and does not mark the
        /// index; run again once the index takes them, it goes on with
        /// what still differs.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_seeding_stops_at_three_failures_in_a_row_and_goes_on_when_run_again() {
            on_indexed_store(|store, _| async move {
                stored(&store, 8).await;
                let before = pending_rows(&store).await;
                let (seeding, second) = unserved(&store).await;
                for bad in ["a002", "a003", "a004"] {
                    second.broken.lock().unwrap().insert(bad.to_string());
                }
                let done = seeding.seed_index(|_, _| {}).await.unwrap();
                assert_eq!((done.indexed, done.failed, done.total), (2, 6, 8));
                let named: Vec<&str> = done.failures.iter().map(|f| f.id.as_str()).collect();
                assert_eq!(named, ["a002", "a003", "a004"]);
                assert_eq!(second.given.load(Ordering::SeqCst), 5, "and no more tried");
                assert!(!second.is_complete());

                second.broken.lock().unwrap().clear();
                let done = seeding.seed_index(|_, _| {}).await.unwrap();
                assert_eq!((done.indexed, done.failed, done.total), (6, 0, 6));
                assert!(second.is_complete());
                assert_eq!(pending_rows(&store).await, before);
            })
            .await;
        }

        /// Invariant 10. A seeding writes as the keeper does: it reads
        /// what is stored again after its write, and writes again what
        /// changed meanwhile, so that a write of its own landing over a
        /// newer text is put right by itself.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_seeding_writes_again_what_changed_under_its_write() {
            on_indexed_store(|store, _| async move {
                stored(&store, 1).await;
                let (seeding, second) = unserved(&store).await;
                second.keeping_back.store(true, Ordering::SeqCst);
                let going = tokio::spawn(async move { seeding.seed_index(|_, _| {}).await });
                second.until_kept_back(1).await;
                rewrite(&store, "a000", "Y").await;
                second.land(0);
                second.answer(0, true);
                second.until_kept_back(2).await;
                second.land(1);
                second.answer(1, true);
                let ended = tokio::time::timeout(Duration::from_secs(10), going).await;
                ended.expect("the seeding ends").unwrap().unwrap();
                assert_eq!(second.content("a000").as_deref(), Some("Y"));
            })
            .await;
        }

        /// Invariant 10. A seeding is refused while a hub serves an index
        /// of the kind, and writes nothing; with no hub serving, it runs.
        /// (By a session lock: a courtesy, which behind a transaction
        /// pooler does nothing.)
        #[tokio::test(flavor = "multi_thread")]
        async fn a_seeding_is_refused_while_a_hub_serves_the_index() {
            on_indexed_store(|store, index| async move {
                stored(&store, 2).await;
                let (seeding, second) = unserved(&store).await;
                store.keep_index_complete(QUICK);
                until("the hub's own index filled", || index.ids_held().len() == 2).await;
                let refused = seeding.seed_index(|_, _| {}).await.unwrap_err();
                assert!(
                    refused.downcast_ref::<SeedRefused>().is_some(),
                    "{refused:#}"
                );
                assert!(format!("{refused}").contains("index/retry"), "{refused}");
                assert!(second.ids_held().is_empty() && second.asked().is_empty());
            })
            .await;
            on_indexed_store(|store, _| async move {
                stored(&store, 2).await;
                let (seeding, second) = unserved(&store).await;
                seeding.seed_index(|_, _| {}).await.unwrap();
                assert_eq!(second.ids_held().len(), 2);
                // And two seedings at once: the second is told the same.
                second.keeping_back.store(true, Ordering::SeqCst);
                rewrite(&store, "a000", "again").await;
                let first = Arc::clone(&seeding);
                let going = tokio::spawn(async move { first.seed_index(|_, _| {}).await });
                second.until_kept_back(1).await;
                assert!(seeding.seed_index(|_, _| {}).await.is_err());
                second.land(0);
                second.answer(0, true);
                going.await.unwrap().unwrap();
            })
            .await;
        }

        /// What the index is waiting to try again is tried at once when
        /// an operator says so.
        #[tokio::test(flavor = "multi_thread")]
        async fn what_is_waiting_to_be_tried_again_is_tried_at_once_on_request() {
            on_indexed_store(|store, index| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 2).await;
                index.broken.lock().unwrap().insert("a001".to_string());
                for _ in 0..8 {
                    let _ = store.round(index.as_ref(), &IndexPace::DEFAULT).await;
                }
                index.broken.lock().unwrap().clear();
                let _ = store.round(index.as_ref(), &IndexPace::DEFAULT).await;
                assert_eq!(
                    index.ids_held(),
                    ["a000"],
                    "not tried again of itself, so soon"
                );
                assert_eq!(store.retry_now().await.unwrap(), 1);
                let _ = store.round(index.as_ref(), &IndexPace::DEFAULT).await;
                assert_eq!(index.ids_held(), ["a000", "a001"]);
            })
            .await;
        }

        // ===== DW-620 B1: every round looks at the index =====
        //
        // dw-620-keeper-b1.md, revision 3. Each compare of a store this
        // small (under one page) asks the index once what differs.

        /// Compares made so far.
        fn compares(index: &Held) -> usize {
            index.compared.load(Ordering::SeqCst)
        }

        /// Rounds until nothing is pending, following the keeper's waits.
        async fn settled(store: &KnowledgeStore, index: &Arc<Held>, hands: &Hands) {
            for _ in 0..30 {
                hands.follow(round(8, store, index).await);
                if noted(store).await.is_empty() {
                    return;
                }
            }
            panic!("still pending: {:?}", noted(store).await);
        }

        /// The last error health gives, or nothing.
        fn said(store: &KnowledgeStore) -> String {
            store.index_state().last_error.unwrap_or_default()
        }

        /// B1, review E2: an index emptied behind the hub is filled again
        /// within three rounds, however many artifacts always fail and are
        /// due in every round before the look.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_emptied_behind_many_failing_artifacts_is_filled_again() {
            for failing in [10usize, 30, 90, 120] {
                timed(|store, _, index, hands| async move {
                    index.mark_complete().await.unwrap();
                    stored(&store, failing + 20).await;
                    settled(&store, &index, &hands).await;
                    let ids = (0..failing).map(|n| format!("a{n:03}"));
                    index.broken.lock().unwrap().extend(ids);
                    let again = format!(
                        "UPDATE artifacts SET content = content || ' again' WHERE id < 'a{failing:03}'"
                    );
                    sql(&store, &again).await;
                    // Long enough for each to have failed and to be due by
                    // its own wait, some in every round.
                    for _ in 0..40 {
                        hands.follow(round(8, &store, &index).await);
                    }
                    index.held.lock().unwrap().clear();
                    for _ in 0..3 {
                        hands.follow(round(8, &store, &index).await);
                    }
                    let held = index.ids_held().len();
                    assert_eq!(
                        held, 20,
                        "with {failing} failing, {held} of 20 held again after 3 rounds"
                    );
                })
                .await;
            }
        }

        /// B1: an index that answers nothing is asked little each round: at
        /// most three writes and one look, which fails, and no compare.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_dead_index_is_asked_little_each_round() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 30).await;
                settled(&store, &index, &hands).await;
                let ids = (0..10).map(|n| format!("a{n:03}"));
                index.broken.lock().unwrap().extend(ids);
                sql(
                    &store,
                    "UPDATE artifacts SET content = content || ' again' WHERE id < 'a010'",
                )
                .await;
                index.down.store(true, Ordering::SeqCst);
                let compared = compares(&index);
                for nth in 0..20 {
                    let asked = |index: &Held| {
                        (
                            index.given.load(Ordering::SeqCst),
                            index.completes.load(Ordering::SeqCst),
                            index.counts.load(Ordering::SeqCst),
                        )
                    };
                    let before = asked(&index);
                    hands.follow(round(8, &store, &index).await);
                    let after = asked(&index);
                    let (given, completes, counts) =
                        (after.0 - before.0, after.1 - before.1, after.2 - before.2);
                    assert!(given <= 3, "round {nth}: {given} writes");
                    assert_eq!(
                        (completes, counts),
                        (1, 0),
                        "round {nth}: one look, which fails before the count"
                    );
                }
                assert_eq!(compares(&index), compared, "a compare started");
                // As Weaviate down: its marker is no request, and the look
                // fails at the count, which is asked once.
                index.down.store(false, Ordering::SeqCst);
                index.read_only.store(true, Ordering::SeqCst);
                index.counts_fail.store(true, Ordering::SeqCst);
                for nth in 0..20 {
                    let before = index.counts.load(Ordering::SeqCst);
                    hands.follow(round(8, &store, &index).await);
                    let counts = index.counts.load(Ordering::SeqCst) - before;
                    assert_eq!(counts, 1, "round {nth}: counts asked of a dead index");
                }
                assert_eq!(compares(&index), compared, "a compare started");
            })
            .await;
        }

        /// B1: an index that takes no writes, emptied, is compared once,
        /// and not again while what that compare noted explains its count.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_that_takes_no_writes_is_compared_once() {
            timed(|store, _, index, hands| async move {
                // No reconcile is due of an index without hashes.
                index.hashless.store(true, Ordering::SeqCst);
                index.mark_complete().await.unwrap();
                stored(&store, 20).await;
                settled(&store, &index, &hands).await;
                index.read_only.store(true, Ordering::SeqCst);
                index.held.lock().unwrap().clear();
                let before = compares(&index);
                for _ in 0..100 {
                    round(8, &store, &index).await;
                    hands.advance(Duration::from_secs(36));
                }
                assert_eq!(compares(&index) - before, 1, "compares in an hour");
            })
            .await;
        }

        /// B1 (M3c): a compare that walks every artifact and is refused
        /// its marker at the end is not run again until the cap is out,
        /// and health says why.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_compare_refused_its_marker_is_held_off_for_the_cap() {
            timed(|store, _, index, hands| async move {
                index.hashless.store(true, Ordering::SeqCst);
                index.refuses_marking.store(true, Ordering::SeqCst);
                stored(&store, 20).await;
                for _ in 0..100 {
                    round(8, &store, &index).await;
                    hands.advance(Duration::from_secs(36));
                }
                let made = compares(&index);
                let most = 1 + 60 / 15;
                assert!((1..=most).contains(&made), "{made} compares in an hour");
                let why = said(&store);
                assert!(why.contains("compared again after"), "no reason: {why}");
            })
            .await;
        }

        /// B1: a round whose work took nothing still compares at start, and
        /// when the reconcile is due; and says it took nothing.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_round_that_takes_nothing_compares_at_start_and_when_due() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 5).await;
                let ids = (0..3).map(|n| format!("a{n:03}"));
                index.broken.lock().unwrap().extend(ids);
                round(8, &store, &index).await;
                assert_eq!(compares(&index), 1, "no compare at start");
                // That compare had the other two taken: nothing to say.
                assert_eq!(index.ids_held(), ["a003", "a004"]);
                hands.advance(PACE.reconcile + cap());
                round(8, &store, &index).await;
                let why = said(&store);
                assert!(why.contains("took none"), "took none, unsaid: {why}");
                assert_eq!(compares(&index), 2, "no reconcile when it was due");
            })
            .await;
        }

        /// B1 (A5): a count no compare explains is compared again only
        /// when the cap is out; meanwhile health says it is out of step,
        /// that searches go by keyword, and why.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_count_no_compare_explains_is_compared_again_only_after_the_cap() {
            timed(|store, _, index, hands| async move {
                index.hashless.store(true, Ordering::SeqCst);
                index.mark_complete().await.unwrap();
                stored(&store, 20).await;
                settled(&store, &index, &hands).await;
                index.miscounts.store(usize::MAX, Ordering::SeqCst);
                let before = compares(&index);
                for nth in 0..100 {
                    round(8, &store, &index).await;
                    if compares(&index) > before {
                        let state = store.index_state();
                        assert_eq!(state.in_step, Some(false), "round {nth}");
                        assert_eq!(state.search, Some("keyword"), "round {nth}");
                        let why = state.last_error.unwrap_or_default();
                        assert!(why.contains("out of step"), "no reason, round {nth}: {why}");
                    }
                    hands.advance(Duration::from_secs(36));
                }
                let made = compares(&index) - before;
                let most = 1 + 60 / 15;
                assert!((1..=most).contains(&made), "{made} compares in an hour");
            })
            .await;
        }

        /// B1 (M3b): a compare that left the count out of step, by one, is
        /// forgiven by the next look that finds it in step: a loss after
        /// that is compared at once, well within the cap.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_look_in_step_lets_the_next_loss_be_compared_at_once() {
            timed(|store, _, index, hands| async move {
                index.hashless.store(true, Ordering::SeqCst);
                index.mark_complete().await.unwrap();
                stored(&store, 20).await;
                settled(&store, &index, &hands).await;
                index.miscounts.store(usize::MAX, Ordering::SeqCst);
                let before = compares(&index);
                round(8, &store, &index).await;
                assert_eq!(compares(&index), before + 1);
                index.miscounts.store(0, Ordering::SeqCst);
                hands.advance(Duration::from_secs(30));
                round(8, &store, &index).await;
                assert_eq!(store.index_state().in_step, Some(true));
                assert_eq!(said(&store), "", "the reason outlived the count");
                index.held.lock().unwrap().clear();
                hands.advance(Duration::from_secs(60));
                round(8, &store, &index).await;
                assert_eq!(
                    compares(&index),
                    before + 2,
                    "the loss waits for the cap of a compare since forgiven"
                );
                assert_eq!(index.ids_held().len(), 20);
            })
            .await;
        }

        /// B1 (M3a): the reconcile is not held off by a compare that ended
        /// badly.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_reconcile_runs_while_other_compares_are_held_off() {
            timed(|store, _, index, hands| async move {
                let pace = IndexPace {
                    reconcile: Duration::from_secs(300),
                    ..PACE
                };
                index.refuses_marking.store(true, Ordering::SeqCst);
                // The store's clock a day off the wall's: what is held off
                // is by the store's.
                hands.advance(Duration::from_secs(86_400));
                stored(&store, 5).await;
                round_at(8, &store, &index, &pace).await;
                assert_eq!(compares(&index), 1, "no compare at start");
                hands.advance(Duration::from_secs(60));
                round_at(8, &store, &index, &pace).await;
                assert_eq!(compares(&index), 1, "compared again within the cap");
                hands.advance(Duration::from_secs(300));
                round_at(8, &store, &index, &pace).await;
                assert_eq!(compares(&index), 2, "the reconcile was held off");
            })
            .await;
        }

        /// B1 (M2): a clock set back counts as time gone by: it does not
        /// stop compares.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_clock_set_back_does_not_stop_compares() {
            timed(|store, _, index, hands| async move {
                index.hashless.store(true, Ordering::SeqCst);
                index.refuses_marking.store(true, Ordering::SeqCst);
                stored(&store, 5).await;
                round(8, &store, &index).await;
                assert_eq!(compares(&index), 1);
                *hands.0.lock().unwrap() -= Duration::from_secs(3600);
                round(8, &store, &index).await;
                assert_eq!(compares(&index), 2, "held off by a compare an hour ahead");
            })
            .await;
        }

        /// B1 (M2): the keeper's wait is never past the reconcile, nor
        /// past the end of a hold, and a clock set back makes it none.
        #[test]
        fn the_keepers_wait_counts_a_clock_set_back_as_time_gone_by() {
            let now = Utc::now();
            let next = now + Duration::from_secs(600);
            let long = Duration::from_secs(600);
            assert_eq!(keeper_wait(next, now, None, &PACE, true), long);
            let well = Compared {
                at: now,
                ended_well: true,
            };
            assert_eq!(keeper_wait(next, now, Some(well), &PACE, false), long);
            let soon = Compared {
                at: now - Duration::from_secs(3500),
                ended_well: true,
            };
            let left = Duration::from_secs(100);
            assert_eq!(keeper_wait(next, now, Some(soon), &PACE, true), left);
            let badly = Compared {
                at: now - Duration::from_secs(800),
                ended_well: false,
            };
            assert_eq!(keeper_wait(next, now, Some(badly), &PACE, false), left);
            let ahead = Compared {
                at: now + Duration::from_secs(3600),
                ended_well: true,
            };
            let none = Duration::ZERO;
            assert_eq!(keeper_wait(next, now, Some(ahead), &PACE, true), none);
            let ahead = Compared {
                ended_well: false,
                ..ahead
            };
            assert_eq!(keeper_wait(next, now, Some(ahead), &PACE, false), none);
            assert_eq!(keeper_wait(now - long, now, None, &PACE, true), none);
        }

        /// B1 (M4): two readings of one look that disagree are named, in
        /// that round and no longer.
        #[tokio::test(flavor = "multi_thread")]
        async fn readings_that_disagree_within_a_look_are_named() {
            timed(|store, _, index, hands| async move {
                index.hashless.store(true, Ordering::SeqCst);
                index.mark_complete().await.unwrap();
                stored(&store, 20).await;
                settled(&store, &index, &hands).await;
                let before = compares(&index);
                index.miscounts.store(1, Ordering::SeqCst);
                round(8, &store, &index).await;
                let why = said(&store);
                assert!(why.contains("disagreed"), "disagreement unnamed: {why}");
                round(8, &store, &index).await;
                assert_eq!(said(&store), "");
                assert_eq!(compares(&index), before);
                // And in a round that took nothing: still two readings, and
                // no compare on one that disagrees.
                index.broken.lock().unwrap().insert("a000".to_string());
                rewrite(&store, "a000", "again").await;
                // At the band's edge with one pending: one too many read
                // once puts the first reading out.
                index.hold("a999", "a stray");
                index.miscounts.store(1, Ordering::SeqCst);
                round(8, &store, &index).await;
                assert_eq!(compares(&index), before, "compared on one reading");
                let why = said(&store);
                assert!(why.contains("took none"), "took none, unsaid: {why}");
                assert!(why.contains("disagreed"), "disagreement unnamed: {why}");
            })
            .await;
        }

        /// The reading after a compare is what health tells. An index that
        /// takes no writes, emptied: the compare notes what it lacks, and in
        /// that round health reads the count in step again (what is pending
        /// explains it), with no reason given. (The review's pin.)
        #[tokio::test(flavor = "multi_thread")]
        async fn health_reads_the_count_after_a_compare() {
            timed(|store, _, index, hands| async move {
                index.hashless.store(true, Ordering::SeqCst);
                index.mark_complete().await.unwrap();
                stored(&store, 20).await;
                settled(&store, &index, &hands).await;
                index.read_only.store(true, Ordering::SeqCst);
                index.held.lock().unwrap().clear();
                let before = compares(&index);
                round(8, &store, &index).await;
                assert_eq!(compares(&index), before + 1, "compared");
                let state = store.index_state();
                assert_eq!(
                    state.in_step,
                    Some(true),
                    "health kept the reading before the compare"
                );
                let why = said(&store);
                assert!(!why.contains("out of step"), "{why}");
            })
            .await;
        }

        /// B1 rule 5: a look that fails, with nothing due, is a round that
        /// took nothing: the next after the fixed wait, not after the watch,
        /// so that the index is looked at again soon after it is back.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_look_that_fails_is_followed_by_the_fixed_wait() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 5).await;
                settled(&store, &index, &hands).await;
                index.down.store(true, Ordering::SeqCst);
                let next = round(8, &store, &index).await;
                assert_eq!(next, Next::After(PACE.retry), "after a failed look");
                index.down.store(false, Ordering::SeqCst);
                index.counts_fail.store(true, Ordering::SeqCst);
                let next = round(8, &store, &index).await;
                assert_eq!(next, Next::After(PACE.retry), "after a count that failed");
            })
            .await;
        }

        /// A round that finds the compare lock held elsewhere, with a
        /// reconcile due, is followed by the fixed wait: what keeps the
        /// keeper from going round as fast as it can, pinned without
        /// timing.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_round_that_finds_the_compare_lock_held_waits_the_retry() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 3).await;
                settled(&store, &index, &hands).await;
                store.ended(hands.now() - PACE.reconcile * 2, true);
                let other = BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false);
                let other = other.await.unwrap();
                let next = round(8, &store, &index).await;
                other.release().await;
                assert_eq!(
                    next,
                    Next::After(PACE.retry),
                    "with the compare lock held elsewhere"
                );
            })
            .await;
        }

        /// A round whose first work took none of what it was given, and
        /// whose compare then had the index take what it lacked, did
        /// something: health does not say it took none, and the next
        /// round is not put off by the fixed wait.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_round_whose_compare_took_something_does_not_say_it_took_none() {
            timed(|store, _, index, hands| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 5).await;
                settled(&store, &index, &hands).await;
                let ids = (0..3).map(|n| format!("a{n:03}"));
                index.broken.lock().unwrap().extend(ids);
                sql(
                    &store,
                    "UPDATE artifacts SET content = content || ' again' WHERE id < 'a003'",
                )
                .await;
                index.held.lock().unwrap().clear();
                let before = compares(&index);
                let next = round(8, &store, &index).await;
                assert_eq!(compares(&index), before + 1, "compared");
                assert_eq!(index.ids_held(), ["a003", "a004"], "filled by the compare");
                let why = said(&store);
                assert!(
                    !why.contains("took none"),
                    "said of a round that filled the index: {why}"
                );
                assert_ne!(
                    next,
                    Next::After(PACE.retry),
                    "put off as a round that did nothing"
                );
            })
            .await;
        }

        /// A reconcile due while another process holds the compare lock:
        /// the keeper waits its fixed wait between looks, and does not go
        /// round as fast as it can for as long as the other's compare
        /// lasts. (The review's repro, at a retry of 200 ms.)
        #[tokio::test(flavor = "multi_thread")]
        async fn a_busy_compare_lock_does_not_spin_the_keeper() {
            on_indexed_store(|store, index| async move {
                index.mark_complete().await.unwrap();
                stored(&store, 3).await;
                store.ended((store.clock)() - chrono::Duration::hours(2), true);
                let other = BackfillLock::acquire(&store.pool, BACKFILL_LOCK, false);
                let other = other.await.unwrap();
                let before = index.completes.load(Ordering::SeqCst);
                store.keep_index_complete(IndexPace {
                    retry: Duration::from_millis(200),
                    watch: Duration::from_secs(600),
                    reconcile: Duration::from_secs(3600),
                    ..QUICK
                });
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let looks = index.completes.load(Ordering::SeqCst) - before;
                other.release().await;
                // The first round, then one each 200 ms: six at most.
                assert!(
                    looks <= 8,
                    "the keeper went round {looks} times in a second while another held the compare lock"
                );
            })
            .await;
        }

        /// B1 (A6): a look that fails starts no compare; the round in which
        /// the index answers again makes exactly one.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_index_that_answers_again_is_compared_once_in_that_round() {
            timed(|store, _, index, hands| async move {
                stored(&store, 5).await;
                index.down.store(true, Ordering::SeqCst);
                for _ in 0..5 {
                    hands.follow(round(8, &store, &index).await);
                }
                assert_eq!(compares(&index), 0);
                index.down.store(false, Ordering::SeqCst);
                round(8, &store, &index).await;
                assert_eq!(compares(&index), 1);
            })
            .await;
        }
    }
}
