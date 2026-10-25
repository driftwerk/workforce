//! The read-shadow kernel (DW-676): a second store asked the same question
//! as the one that answered, in the background, and the two answers
//! compared.
//!
//! The answer the caller already has is the only one it ever sees. What
//! happens to the second question can never make it wait or fail:
//! [`Shadow::observe`] is not `async`, so a caller cannot await it, and it
//! decides at once, in this order, whether to ask at all:
//!
//! 1. the kill switch ([`Shadow::set_enabled`]);
//! 2. the sample rate;
//! 3. a Tokio runtime to run it on;
//! 4. a free permit, taken without waiting: when none is free the sample
//!    is dropped and counted as busy, never queued;
//! 5. the [`Breaker`]: after `trip_after` failures in a row nobody asks
//!    for `rest`, then one probe decides.
//!
//! The permit is taken before the breaker is asked, so a probe the breaker
//! lets through always runs and always reports. Once admitted, the shadow
//! is made and run on a task of its own, holding the permit until it ends,
//! under the deadline; a timeout, an error and a panic each count as a
//! failure. The two rankings are compared ([`RankMetrics`]) and one
//! [`Comparison`] goes to the [`ShadowSink`], which must not block either.
//!
//! A shadow whose future never yields while it blocks a thread is not
//! bounded by the deadline; shadows here are asynchronous clients.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::sync::Semaphore;

use crate::breaker::Breaker;

/// How a shadow is run. Every value is chosen by the caller; there are no
/// defaults here (they belong to the store's config).
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowSettings {
    /// Share of calls asked of the shadow, in `[0, 1]`.
    pub sample: f64,
    /// Shadows that may run at once (at least one).
    pub permits: usize,
    /// How long a shadow may take before it counts as failed.
    pub deadline: Duration,
    /// Failures in a row that open the breaker.
    pub trip_after: u32,
    /// How long the breaker stays open before one probe is let through.
    pub rest: Duration,
    /// Depth the rankings are compared to (at least one).
    pub k: usize,
    /// Persistence of rank-biased overlap, in `(0, 1)`.
    pub rbo_p: f64,
}

impl ShadowSettings {
    /// These settings, or `Err` naming the one the kernel cannot run with.
    /// The one place their bounds are kept: config checks with it too.
    pub fn checked(self) -> eyre::Result<Self> {
        if !(0.0..=1.0).contains(&self.sample) {
            eyre::bail!("shadow sample {} is not within 0 and 1", self.sample);
        }
        if self.permits == 0 {
            eyre::bail!("shadow permits must be at least 1");
        }
        if self.deadline.is_zero() {
            eyre::bail!("a shadow deadline of 0 would fail every shadow");
        }
        if self.trip_after == 0 {
            eyre::bail!("a shadow breaker must trip after at least 1 failure");
        }
        // With the deadline shorter than the rest, the shadows let in
        // before the breaker opened have ended by the time a probe is
        // admitted, so the probe is not refused for want of a permit they
        // hold (observe takes the permit before asking the breaker). A
        // late report from one of them, a timer firing late under load
        // say, is still possible; the breaker's tickets make it harmless.
        if self.deadline >= self.rest {
            eyre::bail!(
                "a shadow deadline ({:?}) must be shorter than the breaker's rest ({:?})",
                self.deadline,
                self.rest
            );
        }
        if self.k == 0 {
            eyre::bail!("shadow rankings must be compared to a depth of at least 1");
        }
        if !(self.rbo_p > 0.0 && self.rbo_p < 1.0) {
            eyre::bail!(
                "rank-biased overlap's p {} is not between 0 and 1",
                self.rbo_p
            );
        }
        Ok(self)
    }
}

/// A ranked answer: ids best first, and the best one's score if the store
/// gives scores.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ranked {
    pub ids: Vec<String>,
    pub top_score: Option<f32>,
    /// How long embedding the question took, apart from the index call,
    /// when this side embedded it; `None` when it was handed a vector.
    pub embed: Option<Duration>,
}

/// What the caller already has: its own answer, how long the index took
/// to give it, and labels to carry into the comparison (for example
/// `same_model`, `alpha`, `mirror_in_step`).
#[derive(Debug, Clone, Default)]
pub struct Observed {
    pub ranked: Ranked,
    /// The index call alone; embedding is in `ranked.embed`.
    pub latency: Duration,
    /// A hash of the question (design r2 §6.3: the text itself is not
    /// kept), so samples of one question can be told apart.
    pub query_hash: Option<String>,
    pub labels: Vec<(&'static str, String)>,
}

/// How the shadow went.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// It answered; the rankings were compared.
    Answered {
        ranked: Ranked,
        metrics: RankMetrics,
    },
    /// It answered with an error.
    Failed(String),
    /// It panicked.
    Panicked,
    /// It had not answered by the deadline.
    TimedOut,
}

/// One sampled question, both sides.
#[derive(Debug, Clone)]
pub struct Comparison {
    pub store: &'static str,
    pub primary: Observed,
    /// From the shadow's task starting to its ending: the asking, under
    /// the deadline. Its wait to be scheduled is in `queued`, not here.
    pub shadow_latency: Duration,
    /// From the shadow being admitted, in [`Shadow::observe`], to its task
    /// starting: the wait for the runtime, outside `shadow_latency`.
    pub queued: Duration,
    /// Of `shadow_latency`, the time spent in the index's own calls
    /// ([`index_call`]) when the shadow answered; `None` when it made none,
    /// or did not answer.
    pub index: Option<Duration>,
    /// Shadows of this store running when this one was admitted, itself
    /// included: 1 when it ran alone.
    pub concurrent: usize,
    pub outcome: Outcome,
    /// Whether this was the breaker's one probe after a rest.
    pub probe: bool,
}

/// Where comparisons go. `record` is called on the shadow's own task, but
/// must still not block: a sink that writes somewhere slow buffers, and
/// drops when its buffer is full.
pub trait ShadowSink: Send + Sync {
    fn record(&self, comparison: Comparison);
}

/// Writes each comparison to the log at debug level.
#[derive(Debug, Default)]
pub struct TracingSink;

impl ShadowSink for TracingSink {
    fn record(&self, c: Comparison) {
        match &c.outcome {
            Outcome::Answered { metrics, ranked } => tracing::debug!(
                store = c.store,
                overlap = metrics.overlap,
                jaccard = metrics.jaccard,
                rbo = metrics.rbo,
                top1 = metrics.top1,
                primary_count = c.primary.ranked.ids.len(),
                shadow_count = ranked.ids.len(),
                primary_ms = c.primary.latency.as_millis() as u64,
                shadow_ms = c.shadow_latency.as_millis() as u64,
                index_ms = c.index.map(|index| index.as_millis() as u64),
                queue_ms = c.queued.as_millis() as u64,
                concurrent = c.concurrent,
                "shadow compared"
            ),
            other => tracing::debug!(
                store = c.store,
                outcome = ?other,
                shadow_ms = c.shadow_latency.as_millis() as u64,
                "shadow did not answer"
            ),
        }
    }
}

/// Keeps every comparison in memory; for tests.
#[derive(Debug, Default)]
pub struct MemorySink {
    held: parking_lot::Mutex<Vec<Comparison>>,
}

impl MemorySink {
    pub fn taken(&self) -> Vec<Comparison> {
        std::mem::take(&mut *self.held.lock())
    }

    pub fn len(&self) -> usize {
        self.held.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ShadowSink for MemorySink {
    fn record(&self, comparison: Comparison) {
        self.held.lock().push(comparison);
    }
}

#[derive(Debug, Default)]
struct Counts {
    offered: AtomicU64,
    paused: AtomicU64,
    unsampled: AtomicU64,
    no_runtime: AtomicU64,
    busy: AtomicU64,
    breaker_open: AtomicU64,
    started: AtomicU64,
    ok: AtomicU64,
    failed: AtomicU64,
    panicked: AtomicU64,
    timed_out: AtomicU64,
}

/// What a [`Shadow`] has done since it was made. Every call to
/// [`Shadow::observe`] is `offered` and lands in exactly one of `paused`,
/// `unsampled`, `no_runtime`, `busy`, `breaker_open` or `started`; every
/// started shadow ends in exactly one of `ok`, `failed`, `panicked` or
/// `timed_out`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShadowCounts {
    pub offered: u64,
    pub paused: u64,
    pub unsampled: u64,
    pub no_runtime: u64,
    pub busy: u64,
    pub breaker_open: u64,
    pub started: u64,
    pub ok: u64,
    pub failed: u64,
    pub panicked: u64,
    pub timed_out: u64,
}

/// One store's read shadow.
pub struct Shadow {
    store: &'static str,
    enabled: AtomicBool,
    sample: f64,
    permits: Arc<Semaphore>,
    /// How many permits there are, to tell how many are held.
    permit_count: usize,
    deadline: Duration,
    breaker: Arc<Breaker>,
    k: usize,
    rbo_p: f64,
    sink: Arc<dyn ShadowSink>,
    counts: Arc<Counts>,
}

impl std::fmt::Debug for Shadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shadow")
            .field("store", &self.store)
            .field("enabled", &self.enabled())
            .field("sample", &self.sample)
            .field("deadline", &self.deadline)
            .field("breaker", &self.breaker.told())
            .finish_non_exhaustive()
    }
}

impl Shadow {
    /// A shadow for `store`, enabled; `Err` naming the setting that
    /// cannot be used.
    pub fn new(
        store: &'static str,
        settings: ShadowSettings,
        sink: Arc<dyn ShadowSink>,
    ) -> eyre::Result<Self> {
        let s = settings.checked()?;
        Ok(Self {
            store,
            enabled: AtomicBool::new(true),
            sample: s.sample,
            permits: Arc::new(Semaphore::new(s.permits)),
            permit_count: s.permits,
            deadline: s.deadline,
            breaker: Arc::new(Breaker::new(s.trip_after, s.rest)),
            k: s.k,
            rbo_p: s.rbo_p,
            sink,
            counts: Arc::default(),
        })
    }

    /// The kill switch: while off, nothing is asked.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// One of the permits [`Self::observe`] draws on, waited for: for work
    /// asked of both stores in the background, outside any request (the
    /// known-item runs), which so shares with the live samples the bound
    /// on how much is asked of vellev at once. `None` while the shadow is
    /// paused. Neither the breaker nor the counts are touched: such work
    /// is not a sample, and is recorded by whoever does it.
    pub async fn permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        if !self.enabled() {
            return None;
        }
        // The semaphore is never closed.
        Arc::clone(&self.permits).acquire_owned().await.ok()
    }

    /// How long a shadow may take.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// The breaker's state, for status reports.
    pub fn breaker_told(&self) -> &'static str {
        self.breaker.told()
    }

    pub fn counts(&self) -> ShadowCounts {
        let c = &*self.counts;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ShadowCounts {
            offered: get(&c.offered),
            paused: get(&c.paused),
            unsampled: get(&c.unsampled),
            no_runtime: get(&c.no_runtime),
            busy: get(&c.busy),
            breaker_open: get(&c.breaker_open),
            started: get(&c.started),
            ok: get(&c.ok),
            failed: get(&c.failed),
            panicked: get(&c.panicked),
            timed_out: get(&c.timed_out),
        }
    }

    fn sampled(&self) -> bool {
        // 1.0 always and 0.0 never, exactly.
        self.sample >= 1.0 || (self.sample > 0.0 && rand::random::<f64>() < self.sample)
    }

    /// Ask the shadow the question `primary` already answered, in the
    /// background, and compare. Returns at once whatever the shadow does;
    /// `shadow` itself is called on the shadow's task, not here.
    pub fn observe<F, Fut>(&self, primary: Observed, shadow: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = eyre::Result<Ranked>> + Send + 'static,
    {
        let counts = &self.counts;
        counts.offered.fetch_add(1, Ordering::Relaxed);
        if !self.enabled() {
            counts.paused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if !self.sampled() {
            counts.unsampled.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            counts.no_runtime.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            counts.busy.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // Taken after the permit, so this shadow is among them.
        let concurrent = self.permit_count - self.permits.available_permits();
        let Some(ticket) = self.breaker.admit() else {
            counts.breaker_open.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let probe = ticket.is_probe();
        counts.started.fetch_add(1, Ordering::Relaxed);

        let store = self.store;
        let deadline = self.deadline;
        let breaker = Arc::clone(&self.breaker);
        let sink = Arc::clone(&self.sink);
        let counts = Arc::clone(&self.counts);
        let (k, rbo_p) = (self.k, self.rbo_p);
        let admitted = Instant::now();
        runtime.spawn(async move {
            let _permit = permit;
            let began = Instant::now();
            let queued = began - admitted;
            let timed = Arc::new(IndexTime::default());
            let asked = AssertUnwindSafe(
                INDEX_TIME.scope(Arc::clone(&timed), async move { shadow().await }),
            )
            .catch_unwind();
            let answered = tokio::time::timeout(deadline, asked).await;
            let shadow_latency = began.elapsed();
            let outcome = match answered {
                Ok(Ok(Ok(ranked))) => {
                    let metrics = RankMetrics::of(&primary.ranked.ids, &ranked.ids, k, rbo_p);
                    Outcome::Answered { ranked, metrics }
                }
                Ok(Ok(Err(error))) => Outcome::Failed(format!("{error:#}")),
                Ok(Err(_panic)) => Outcome::Panicked,
                Err(_elapsed) => Outcome::TimedOut,
            };
            // The ticket counts only if the breaker has not changed state
            // since this shadow was let in: a late answer to a question
            // asked before it opened, or before a probe closed it, is
            // dropped, so the probe decides.
            let tally = match outcome {
                Outcome::Answered { .. } => {
                    breaker.succeeded(ticket);
                    &counts.ok
                }
                Outcome::Failed(_) | Outcome::Panicked | Outcome::TimedOut => {
                    if breaker.failed(ticket) {
                        tracing::warn!(store, rest = ?breaker.rest(), "shadow breaker opened");
                    }
                    match outcome {
                        Outcome::Failed(_) => &counts.failed,
                        Outcome::Panicked => &counts.panicked,
                        _ => &counts.timed_out,
                    }
                }
            };
            tally.fetch_add(1, Ordering::Relaxed);
            let index = match outcome {
                Outcome::Answered { .. } => timed.total(),
                _ => None,
            };
            sink.record(Comparison {
                store,
                primary,
                shadow_latency,
                queued,
                index,
                concurrent,
                outcome,
                probe,
            });
        });
    }
}

/// The time a shadow's task spent in [`index_call`]s, summed: `None` until
/// one ends.
#[derive(Debug, Default)]
struct IndexTime(parking_lot::Mutex<Option<Duration>>);

impl IndexTime {
    fn add(&self, took: Duration) {
        let mut total = self.0.lock();
        *total = Some(total.unwrap_or_default() + took);
    }

    fn total(&self) -> Option<Duration> {
        *self.0.lock()
    }
}

tokio::task_local! {
    /// Set for the run of each shadow's future, and nowhere else.
    static INDEX_TIME: Arc<IndexTime>;
}

/// Await `call`, a call to an index (a search, say), timing it when it is
/// made by a shadow: the shadow's [`Comparison::index`] is the sum of the
/// calls its task made, so that the rest of its time (embedding, decoding,
/// waiting on anything else) can be told from the index's.
///
/// Elsewhere, the primary's calls included, it is `call` alone. A call
/// made on a task the shadow spawned is not counted: the timing is the
/// shadow's own task's.
pub async fn index_call<F: Future>(call: F) -> F::Output {
    let began = Instant::now();
    let out = call.await;
    let took = began.elapsed();
    // Not in a shadow: nothing to add it to.
    let _ = INDEX_TIME.try_with(|timed| timed.add(took));
    out
}

/// How two rankings of ids agree to a depth `k`.
///
/// Each list is cut to its first `k` ids, an id repeated within a list
/// counting once, at its first rank. Two empty lists agree completely; an
/// empty list and a non-empty one not at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankMetrics {
    pub k: usize,
    /// Ids in both, over the longer list's length (so an id one side has
    /// and the other lacks counts against agreement).
    pub overlap: f64,
    /// Ids in both, over ids in either.
    pub jaccard: f64,
    /// Rank-biased overlap, extrapolated (Webber, Moffat and Zobel 2010,
    /// equation 32), with persistence `p`.
    pub rbo: f64,
    /// Both put the same id first.
    pub top1: bool,
    pub empty_primary_only: bool,
    pub empty_shadow_only: bool,
}

fn cut(ids: &[String], k: usize) -> Vec<&str> {
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .map(String::as_str)
        .filter(|id| seen.insert(*id))
        .take(k)
        .collect()
}

impl RankMetrics {
    pub fn of(primary: &[String], shadow: &[String], k: usize, p: f64) -> Self {
        let a = cut(primary, k);
        let b = cut(shadow, k);
        let empty_primary_only = a.is_empty() && !b.is_empty();
        let empty_shadow_only = b.is_empty() && !a.is_empty();
        if a.is_empty() && b.is_empty() {
            return Self {
                k,
                overlap: 1.0,
                jaccard: 1.0,
                rbo: 1.0,
                top1: true,
                empty_primary_only,
                empty_shadow_only,
            };
        }
        let in_a: std::collections::HashSet<&str> = a.iter().copied().collect();
        let both = b.iter().filter(|id| in_a.contains(*id)).count();
        let longer = a.len().max(b.len());
        let either = a.len() + b.len() - both;
        Self {
            k,
            overlap: both as f64 / longer as f64,
            jaccard: both as f64 / either as f64,
            rbo: rbo_ext(&a, &b, p),
            top1: a.first() == b.first(),
            empty_primary_only,
            empty_shadow_only,
        }
    }
}

/// Extrapolated rank-biased overlap of two lists without repeats, of
/// possibly different lengths (Webber, Moffat and Zobel 2010, eq. 32):
///
/// ```text
/// RBO_ext = (1-p)/p · ( Σ_{d=1..l} X_d/d · p^d + Σ_{d=s+1..l} X_s(d-s)/(s·d) · p^d )
///           + ( (X_l - X_s)/l + X_s/s ) · p^l
/// ```
///
/// where `s` and `l` are the shorter and longer lengths and `X_d` is the
/// overlap of the two lists' first `d` ids (the shorter list whole, past
/// its end). 1 for identical lists, 0 for disjoint ones or when one is
/// empty and the other not.
fn rbo_ext(a: &[&str], b: &[&str], p: f64) -> f64 {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let (s, l) = (short.len(), long.len());
    if s == 0 {
        return if l == 0 { 1.0 } else { 0.0 };
    }
    let mut seen_short = std::collections::HashSet::new();
    let mut seen_long = std::collections::HashSet::new();
    let mut x = 0usize;
    let mut x_s = 0usize;
    let mut sum = 0.0;
    for d in 1..=l {
        // The short list's id at d is matched against the long list above
        // d, and the long list's id at d against the short list to d
        // inclusive, so a pair at the same depth counts once.
        if let Some(id) = short.get(d - 1).copied() {
            if seen_long.contains(id) {
                x += 1;
            }
            seen_short.insert(id);
        }
        let deeper_long = long[d - 1];
        if seen_short.contains(deeper_long) {
            x += 1;
        }
        seen_long.insert(deeper_long);
        let weight = p.powi(d as i32);
        sum += x as f64 / d as f64 * weight;
        if d == s {
            x_s = x;
        }
        if d > s {
            sum += x_s as f64 * (d - s) as f64 / (s * d) as f64 * weight;
        }
    }
    let x_l = x;
    (1.0 - p) / p * sum + ((x_l - x_s) as f64 / l as f64 + x_s as f64 / s as f64) * p.powi(l as i32)
}

#[cfg(test)]
mod tests;
