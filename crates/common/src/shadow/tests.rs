use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

/// How soon `observe` must return, whatever the shadow does: far above the
/// microseconds it takes, so a loaded machine's scheduling cannot fail it,
/// and far below [`BLOCKS`], so a shadow made on the caller's thread does.
const PROMPT: Duration = Duration::from_millis(250);

/// How long one test shadow blocks the thread that makes it.
const BLOCKS: Duration = Duration::from_secs(2);

fn settings() -> ShadowSettings {
    ShadowSettings {
        sample: 1.0,
        permits: 4,
        deadline: Duration::from_secs(10),
        trip_after: 3,
        rest: Duration::from_secs(60),
        k: 10,
        rbo_p: 0.9,
    }
}

fn shadow(settings: ShadowSettings) -> (Shadow, Arc<MemorySink>) {
    let sink = Arc::new(MemorySink::default());
    let shadow = Shadow::new("test", settings, sink.clone()).expect("settings are fine");
    (shadow, sink)
}

/// A shadow's question, boxed so that different ones go in one list.
type Ask = Box<dyn FnOnce() -> futures::future::BoxFuture<'static, eyre::Result<Ranked>> + Send>;

fn ids(of: &[&str]) -> Vec<String> {
    of.iter().map(|s| s.to_string()).collect()
}

fn observed(of: &[&str]) -> Observed {
    Observed {
        ranked: Ranked {
            ids: ids(of),
            top_score: Some(0.5),
            embed: None,
        },
        query_hash: Some("q".into()),
        latency: Duration::from_millis(7),
        labels: vec![("same_model", "true".into())],
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "waited for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Times one `observe` and asserts it returned promptly.
fn prompt(
    shadow: &Shadow,
    ask: impl FnOnce() -> futures::future::BoxFuture<'static, eyre::Result<Ranked>> + Send + 'static,
) {
    let began = Instant::now();
    shadow.observe(observed(&["a"]), ask);
    let took = began.elapsed();
    assert!(took < PROMPT, "observe took {took:?}");
}

fn answers(of: &'static [&'static str]) -> Ask {
    Box::new(move || {
        async move {
            Ok(Ranked {
                ids: ids(of),
                top_score: None,
                embed: None,
            })
        }
        .boxed()
    })
}

fn fails() -> Ask {
    Box::new(|| async { Err(eyre::eyre!("vellev said no")) }.boxed())
}

fn hangs() -> Ask {
    Box::new(|| futures::future::pending().boxed())
}

// ---- The caller never waits ------------------------------------------------

/// A shadow that hangs, fails, panics, or blocks its thread before it has
/// even made its future: the caller's `observe` returns at once each time,
/// and each ends counted as it went.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_or_failing_shadow_never_delays_the_caller() {
    let (shadow, sink) = shadow(ShadowSettings {
        permits: 8,
        deadline: Duration::from_millis(300),
        trip_after: 100,
        ..settings()
    });

    prompt(&shadow, hangs());
    prompt(&shadow, fails());
    prompt(&shadow, || {
        async { panic!("in the shadow's future") }.boxed()
    });
    prompt(&shadow, || panic!("making the shadow's future"));
    prompt(&shadow, || {
        // Blocks whatever thread calls it: never the caller's.
        std::thread::sleep(BLOCKS);
        async { Ok(Ranked::default()) }.boxed()
    });

    until("every shadow ended", || sink.len() == 5).await;
    let counts = shadow.counts();
    assert_eq!(
        (
            counts.started,
            counts.timed_out,
            counts.failed,
            counts.panicked,
            counts.ok
        ),
        (5, 1, 1, 2, 1),
        "{counts:?}"
    );
}

/// The caller's own answer goes into the comparison untouched, and a
/// shadow that does not answer is recorded as not answering, not dropped.
#[tokio::test]
async fn a_shadow_that_does_not_answer_is_recorded_as_such() {
    let (shadow, sink) = shadow(ShadowSettings {
        deadline: Duration::from_millis(20),
        ..settings()
    });
    shadow.observe(observed(&["a", "b"]), hangs());
    until("the deadline", || sink.len() == 1).await;
    let c = sink.taken().remove(0);
    assert_eq!(c.outcome, Outcome::TimedOut);
    assert!(c.shadow_latency >= Duration::from_millis(20));
    assert_eq!(c.primary.ranked.ids, ids(&["a", "b"]));
    assert_eq!(c.primary.latency, Duration::from_millis(7));
    assert_eq!(c.store, "test");
}

// ---- Permits ----------------------------------------------------------------

/// With every permit held, a further sample is dropped and counted as
/// busy: it never runs, then or later. Never more shadows run at once than
/// there are permits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permits_are_bounded_and_a_busy_sample_is_dropped_not_queued() {
    let (shadow, sink) = shadow(ShadowSettings {
        permits: 2,
        ..settings()
    });
    let running = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicUsize::new(0));
    // Permits are stored, so a shadow that waits after they are added
    // still finds them.
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let held = |n: usize| {
        for _ in 0..n {
            let (running, most, ran, release) =
                (running.clone(), most.clone(), ran.clone(), release.clone());
            prompt(&shadow, move || {
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    let _go = release.acquire().await.expect("never closed");
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(Ranked::default())
                }
                .boxed()
            });
        }
    };
    // Two take both permits, and are running before anything else is
    // asked: a permit let go once a shadow has started would be free by
    // now, and the eight below would run.
    held(2);
    until("two running", || running.load(Ordering::SeqCst) == 2).await;
    held(8);
    // Give anything that took a permit the chance to start.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counts = shadow.counts();
    assert_eq!((counts.started, counts.busy), (2, 8), "{counts:?}");
    assert_eq!(
        most.load(Ordering::SeqCst),
        2,
        "more ran at once than permits"
    );

    release.add_permits(2);
    until("both ended", || sink.len() == 2).await;
    // Give anything queued the chance to run; nothing is.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ran.load(Ordering::SeqCst), 2, "a dropped sample ran later");
    assert_eq!(most.load(Ordering::SeqCst), 2);
    assert_eq!(sink.len(), 2);

    // The permits are back.
    shadow.observe(observed(&[]), answers(&[]));
    until("a third", || sink.len() == 3).await;
    assert_eq!(shadow.counts().busy, 8);
}

/// A shadow cut off by its deadline gives its permit back.
#[tokio::test]
async fn a_timed_out_shadow_gives_its_permit_back() {
    let (shadow, sink) = shadow(ShadowSettings {
        permits: 1,
        deadline: Duration::from_millis(20),
        ..settings()
    });
    shadow.observe(observed(&["a"]), hangs());
    until("the deadline", || shadow.counts().timed_out == 1).await;
    shadow.observe(observed(&["a"]), answers(&["a"]));
    until("the second", || sink.len() == 2).await;
    let counts = shadow.counts();
    assert_eq!(
        (counts.started, counts.busy, counts.ok),
        (2, 0, 1),
        "{counts:?}"
    );
}

// ---- The breaker --------------------------------------------------------------

/// `trip_after` failures in a row open it: nobody is asked during the rest.
/// After the rest one probe is let through; failing, it opens again;
/// answering, it closes.
#[tokio::test]
async fn the_breaker_opens_after_failures_in_a_row_and_one_probe_decides() {
    let rest = Duration::from_millis(150);
    let (shadow, sink) = shadow(ShadowSettings {
        trip_after: 2,
        rest,
        deadline: Duration::from_millis(100),
        ..settings()
    });

    shadow.observe(observed(&["a"]), fails());
    until("one failure", || sink.len() == 1).await;
    assert_eq!(
        shadow.breaker_told(),
        "closed",
        "one failure does not open it"
    );
    shadow.observe(observed(&["a"]), fails());
    until("two failures", || sink.len() == 2).await;
    assert_eq!(shadow.breaker_told(), "open");

    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    shadow.observe(observed(&["a"]), move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Ok(Ranked::default()) }.boxed()
    });
    assert_eq!(shadow.counts().breaker_open, 1);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(asked.load(Ordering::SeqCst), 0, "asked while open");

    // The rest over: one probe, which fails, and it is open again.
    tokio::time::sleep(rest).await;
    shadow.observe(observed(&["a"]), fails());
    until("the probe", || sink.len() == 3).await;
    assert!(sink.taken().last().unwrap().probe);
    assert_eq!(shadow.breaker_told(), "open");

    // The next rest over: a probe that answers closes it.
    tokio::time::sleep(rest + Duration::from_millis(20)).await;
    shadow.observe(observed(&["a"]), answers(&["a"]));
    until("the second probe", || sink.len() == 1).await;
    assert!(sink.taken()[0].probe);
    assert_eq!(shadow.breaker_told(), "closed");
    shadow.observe(observed(&["a"]), answers(&["a"]));
    until("asked again", || sink.len() == 1).await;
    assert!(!sink.taken()[0].probe);
    assert_eq!(shadow.counts().breaker_open, 1);
}

/// A timeout and a panic count as failures, as an error does.
#[tokio::test]
async fn a_timeout_or_a_panic_opens_the_breaker_as_an_error_does() {
    let (shadow, sink) = shadow(ShadowSettings {
        deadline: Duration::from_millis(20),
        trip_after: 2,
        ..settings()
    });
    shadow.observe(observed(&["a"]), hangs());
    until("timed out", || sink.len() == 1).await;
    shadow.observe(observed(&["a"]), || async { panic!("boom") }.boxed());
    until("panicked", || sink.len() == 2).await;
    assert_eq!(shadow.breaker_told(), "open");
}

/// A success between failures starts the count again.
#[tokio::test]
async fn failures_must_be_in_a_row_to_open_the_breaker() {
    let (shadow, sink) = shadow(ShadowSettings {
        trip_after: 2,
        ..settings()
    });
    for (n, ask) in [fails(), answers(&["a"]), fails()].into_iter().enumerate() {
        shadow.observe(observed(&["a"]), ask);
        until("each", || sink.len() == n + 1).await;
    }
    assert_eq!(shadow.breaker_told(), "closed");
}

// ---- The kill switch, sampling, no runtime --------------------------------------

#[tokio::test]
async fn the_kill_switch_stops_every_shadow_and_turns_back_on() {
    let (shadow, sink) = shadow(settings());
    shadow.set_enabled(false);
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    shadow.observe(observed(&["a"]), move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { Ok(Ranked::default()) }.boxed()
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(shadow.counts().paused, 1);
    assert!(sink.is_empty());

    shadow.set_enabled(true);
    shadow.observe(observed(&["a"]), answers(&["a"]));
    until("asked", || sink.len() == 1).await;
}

#[tokio::test]
async fn a_sample_rate_of_zero_asks_never_and_one_always() {
    let (never, _) = shadow(ShadowSettings {
        sample: 0.0,
        ..settings()
    });
    let (always, _) = shadow(ShadowSettings {
        sample: 1.0,
        permits: 1000,
        ..settings()
    });
    for _ in 0..500 {
        never.observe(observed(&["a"]), answers(&["a"]));
        always.observe(observed(&["a"]), answers(&["a"]));
    }
    assert_eq!((never.counts().unsampled, never.counts().started), (500, 0));
    assert_eq!(
        (always.counts().unsampled, always.counts().started),
        (0, 500)
    );
}

#[tokio::test]
async fn a_sample_rate_between_asks_about_that_share() {
    let (half, _) = shadow(ShadowSettings {
        sample: 0.25,
        permits: 10_000,
        ..settings()
    });
    for _ in 0..8_000 {
        half.observe(observed(&["a"]), answers(&["a"]));
    }
    // 2,000 expected; the standard deviation is about 39.
    let started = half.counts().started;
    assert!((1_800..=2_200).contains(&started), "{started} of 8000");
}

/// Outside a Tokio runtime there is nothing to run it on: dropped and
/// counted, never a panic.
#[test]
fn outside_a_runtime_a_sample_is_dropped_and_counted() {
    let (shadow, _) = shadow(settings());
    shadow.observe(observed(&["a"]), answers(&["a"]));
    let counts = shadow.counts();
    assert_eq!((counts.no_runtime, counts.started), (1, 0), "{counts:?}");
    assert_eq!(shadow.breaker_told(), "closed");
}

/// Every call lands in exactly one place.
#[tokio::test]
async fn every_call_is_counted_once() {
    let (shadow, sink) = shadow(ShadowSettings {
        permits: 1,
        ..settings()
    });
    shadow.observe(observed(&["a"]), hangs());
    shadow.observe(observed(&["a"]), answers(&["a"]));
    shadow.set_enabled(false);
    shadow.observe(observed(&["a"]), answers(&["a"]));
    let c = shadow.counts();
    assert_eq!(c.offered, 3);
    assert_eq!(
        c.paused + c.unsampled + c.no_runtime + c.busy + c.breaker_open + c.started,
        c.offered
    );
    assert!(sink.is_empty());
}

#[test]
fn settings_that_cannot_be_used_are_refused_by_name() {
    let refused = |s: ShadowSettings| {
        Shadow::new("test", s, Arc::new(TracingSink))
            .unwrap_err()
            .to_string()
    };
    assert!(refused(ShadowSettings {
        sample: 1.5,
        ..settings()
    })
    .contains("sample"));
    assert!(refused(ShadowSettings {
        sample: f64::NAN,
        ..settings()
    })
    .contains("sample"));
    assert!(refused(ShadowSettings {
        permits: 0,
        ..settings()
    })
    .contains("permits"));
    assert!(refused(ShadowSettings {
        deadline: Duration::ZERO,
        ..settings()
    })
    .contains("deadline"));
    assert!(refused(ShadowSettings {
        trip_after: 0,
        ..settings()
    })
    .contains("breaker"));
    assert!(refused(ShadowSettings { k: 0, ..settings() }).contains("depth"));
    assert!(refused(ShadowSettings {
        rbo_p: 1.0,
        ..settings()
    })
    .contains("p "));
    // A deadline not shorter than the rest could leave shadows let in
    // before the breaker opened holding every permit when the probe comes.
    for deadline in [Duration::from_secs(60), Duration::from_secs(61)] {
        let said = refused(ShadowSettings {
            deadline,
            rest: Duration::from_secs(60),
            ..settings()
        });
        assert!(said.contains("shorter than the breaker's rest"), "{said}");
    }
    assert!(refused(ShadowSettings {
        rbo_p: 0.0,
        ..settings()
    })
    .contains("p "));
}

// ---- Rank metrics on known rankings ----------------------------------------------

fn metrics(a: &[&str], b: &[&str], k: usize, p: f64) -> RankMetrics {
    RankMetrics::of(&ids(a), &ids(b), k, p)
}

fn close(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-9, "{got} is not {want}");
}

/// A = [a b c], B = [b a e] at k 3: two in both of three, of four in
/// either; different firsts.
#[test]
fn overlap_jaccard_and_top1_on_known_rankings() {
    let m = metrics(&["a", "b", "c", "d"], &["b", "a", "e"], 3, 0.9);
    close(m.overlap, 2.0 / 3.0);
    close(m.jaccard, 2.0 / 4.0);
    assert!(!m.top1);
    assert!(!m.empty_primary_only && !m.empty_shadow_only);

    let same = metrics(&["a", "b", "c"], &["a", "b", "c"], 10, 0.9);
    close(same.overlap, 1.0);
    close(same.jaccard, 1.0);
    close(same.rbo, 1.0);
    assert!(same.top1);

    let apart = metrics(&["a", "b"], &["c", "d"], 10, 0.9);
    close(apart.overlap, 0.0);
    close(apart.jaccard, 0.0);
    close(apart.rbo, 0.0);
}

/// Shorter than k: an id one side has and the other lacks counts against
/// agreement (over the longer list, not k and not the shorter).
#[test]
fn lists_shorter_than_k_are_compared_over_the_longer() {
    let m = metrics(&["a", "b"], &["a", "b", "c"], 10, 0.9);
    close(m.overlap, 2.0 / 3.0);
    close(m.jaccard, 2.0 / 3.0);
    assert!(m.top1);
}

/// An id repeated in one list counts once, at its first rank.
#[test]
fn a_repeated_id_counts_once_at_its_first_rank() {
    let m = metrics(&["a", "a", "b"], &["a", "b"], 2, 0.9);
    close(m.overlap, 1.0);
    close(m.rbo, 1.0);
}

#[test]
fn empty_answers() {
    let both = metrics(&[], &[], 10, 0.9);
    close(both.overlap, 1.0);
    close(both.rbo, 1.0);
    assert!(both.top1 && !both.empty_primary_only && !both.empty_shadow_only);

    let shadow_only = metrics(&["a"], &[], 10, 0.9);
    close(shadow_only.overlap, 0.0);
    close(shadow_only.rbo, 0.0);
    assert!(!shadow_only.top1 && shadow_only.empty_shadow_only && !shadow_only.empty_primary_only);

    let primary_only = metrics(&[], &["a"], 10, 0.9);
    assert!(primary_only.empty_primary_only && !primary_only.empty_shadow_only);
}

/// Rank-biased overlap, worked by hand from Webber et al.'s equation 32.
#[test]
fn rank_biased_overlap_on_known_rankings() {
    // [a b] / [b a], p 0.9: X1 0, X2 2.
    // (0.1/0.9)(0 + 2/2 * 0.81) + (2/2) * 0.81 = 0.09 + 0.81.
    close(metrics(&["a", "b"], &["b", "a"], 10, 0.9).rbo, 0.9);

    // [a b c] / [a c b], p 0.9: X1 1, X2 1, X3 3.
    // (0.1/0.9)(0.9 + 0.405 + 0.729) + 0.729 = 0.226 + 0.729.
    close(
        metrics(&["a", "b", "c"], &["a", "c", "b"], 10, 0.9).rbo,
        0.955,
    );

    // [a b] / [c a], p 0.5: X1 0, X2 1.
    // (0.5/0.5)(0 + 1/2 * 0.25) + (1/2) * 0.25 = 0.125 + 0.125.
    close(metrics(&["a", "b"], &["c", "a"], 10, 0.5).rbo, 0.25);

    // Uneven: [a] / [a b], p 0.5: s 1, l 2, X1 1, X2 1.
    // (1)(1*0.5 + 1/2*0.25 + 1*(1)/(1*2)*0.25) + (0/2 + 1/1) * 0.25
    //   = 0.75 + 0.25: the short list is taken to go on as the long.
    close(metrics(&["a"], &["a", "b"], 10, 0.5).rbo, 1.0);

    // Uneven, disagreeing: [x] / [a x], p 0.5: X1 0, X2 1, X_s 0.
    // (1)(0 + 1/2*0.25 + 0) + (1/2 + 0) * 0.25 = 0.125 + 0.125.
    close(metrics(&["x"], &["a", "x"], 10, 0.5).rbo, 0.25);
}

/// Cut to k before anything is compared.
#[test]
fn rankings_are_cut_to_k() {
    let m = metrics(&["a", "b", "x"], &["a", "b", "y"], 2, 0.9);
    close(m.overlap, 1.0);
    close(m.rbo, 1.0);
}

/// The comparison handed to the sink carries the metrics, the shadow's
/// ranking and the caller's labels.
#[tokio::test]
async fn the_sink_gets_the_comparison() {
    let (shadow, sink) = shadow(settings());
    shadow.observe(observed(&["a", "b", "c"]), answers(&["a", "c", "b"]));
    until("compared", || sink.len() == 1).await;
    let c = sink.taken().remove(0);
    let Outcome::Answered { ranked, metrics } = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    assert_eq!(ranked.ids, ids(&["a", "c", "b"]));
    close(metrics.rbo, 0.955);
    assert!(metrics.top1);
    assert_eq!(c.primary.labels, vec![("same_model", "true".to_string())]);
    assert!(!c.probe);
}

/// A shadow admitted while the breaker was closed, answering after it
/// opened, says nothing of how things stand now: the breaker stays open.
/// (The #418 review's `review_a_late_success_from_before_the_trip_closes_the_breaker`,
/// red against the breaker before DW-620's tickets.)
#[tokio::test]
async fn a_late_success_from_before_the_trip_leaves_the_breaker_open() {
    let (shadow, sink) = shadow(ShadowSettings {
        trip_after: 1,
        rest: Duration::from_secs(60),
        ..settings()
    });
    let go = Arc::new(tokio::sync::Semaphore::new(0));
    let waits = go.clone();
    shadow.observe(observed(&["a"]), move || {
        async move {
            let _go = waits.acquire().await.expect("never closed");
            Ok(Ranked::default())
        }
        .boxed()
    });
    shadow.observe(observed(&["a"]), fails());
    until("the failure", || sink.len() == 1).await;
    assert_eq!(shadow.breaker_told(), "open");
    go.add_permits(1);
    until("the late success", || sink.len() == 2).await;
    assert_eq!(shadow.breaker_told(), "open", "a late success closed it");
}

// ---- Where a shadow's time goes ---------------------------------------------

/// The wait for the runtime to start a shadow's task is `queued`, and is
/// not in `shadow_latency`: here the one thread the runtime has is held
/// past the shadow being admitted, and the shadow itself answers at once.
#[tokio::test(flavor = "current_thread")]
async fn the_wait_to_be_scheduled_is_queued_not_shadow_latency() {
    const HELD: Duration = Duration::from_millis(80);
    let (shadow, sink) = shadow(settings());
    shadow.observe(observed(&["a"]), answers(&["a"]));
    // The shadow's task cannot start until this one yields.
    std::thread::sleep(HELD);
    until("the shadow ended", || sink.len() == 1).await;
    let c = sink.taken().remove(0);
    assert!(c.queued >= HELD, "queued {:?}", c.queued);
    assert!(
        c.shadow_latency < HELD,
        "shadow_latency {:?} counts the wait to be scheduled",
        c.shadow_latency
    );
}

/// `concurrent` is the shadows running when one was admitted, itself
/// among them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_is_the_shadows_running_when_one_was_admitted() {
    let (alone, sink) = shadow(settings());
    alone.observe(observed(&["a"]), answers(&["a"]));
    until("the lone shadow ended", || sink.len() == 1).await;
    assert_eq!(sink.taken()[0].concurrent, 1, "alone");

    // A shadow of its own: a permit is given back only after its holder
    // records, so the lone one's may not be back yet.
    let (shadow, sink) = shadow(settings());
    let mut holds = Vec::new();
    for _ in 0..2 {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        holds.push(tx);
        shadow.observe(observed(&["a"]), move || {
            async move {
                let _ = rx.await;
                Ok(Ranked::default())
            }
            .boxed()
        });
    }
    shadow.observe(observed(&["a"]), answers(&["a"]));
    until("the third shadow ended", || sink.len() == 1).await;
    assert_eq!(sink.taken()[0].concurrent, 3, "two held, and itself");
    drop(holds);
    until("the held shadows ended", || sink.len() == 2).await;
    let held: Vec<usize> = sink.taken().iter().map(|c| c.concurrent).collect();
    assert_eq!(held, vec![1, 2], "each counted the ones before it");
}

/// `index` is the sum of the shadow's index calls, and nothing else of
/// its time: here 2 calls of `CALL` each, after `ELSE` of other work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_is_the_sum_of_the_shadows_index_calls_alone() {
    const CALL: Duration = Duration::from_millis(30);
    const ELSE: Duration = Duration::from_millis(60);
    let (shadow, sink) = shadow(settings());
    shadow.observe(observed(&["a"]), || {
        async {
            tokio::time::sleep(ELSE).await;
            index_call(tokio::time::sleep(CALL)).await;
            index_call(tokio::time::sleep(CALL)).await;
            Ok(Ranked::default())
        }
        .boxed()
    });
    until("the shadow ended", || sink.len() == 1).await;
    let c = sink.taken().remove(0);
    let index = c.index.expect("index calls were made");
    assert!(index >= 2 * CALL, "both calls: {index:?}");
    assert!(
        index + ELSE <= c.shadow_latency,
        "index {index:?} counts the other work (shadow {:?})",
        c.shadow_latency
    );
}

/// No index call, no index time; one that did not answer has none either;
/// and outside a shadow, an index call is the call alone.
#[tokio::test]
async fn index_is_none_without_index_calls_or_an_answer() {
    let (shadow, sink) = shadow(settings());
    shadow.observe(observed(&["a"]), answers(&["a"]));
    shadow.observe(observed(&["a"]), || {
        async {
            index_call(async {}).await;
            Err(eyre::eyre!("vellev said no"))
        }
        .boxed()
    });
    until("both ended", || sink.len() == 2).await;
    for c in sink.taken() {
        assert_eq!(c.index, None, "{:?}", c.outcome);
    }
    assert_eq!(index_call(async { 7 }).await, 7);
}

/// `queued` ends when the shadow's task starts: the shadow's own run is
/// not in it. Here the shadow takes `RUN`, far longer than the wait.
#[tokio::test(flavor = "current_thread")]
async fn queued_stops_when_the_shadow_starts() {
    const HELD: Duration = Duration::from_millis(50);
    const RUN: Duration = Duration::from_millis(300);
    let (shadow, sink) = shadow(settings());
    shadow.observe(observed(&["a"]), || {
        async {
            tokio::time::sleep(RUN).await;
            Ok(Ranked::default())
        }
        .boxed()
    });
    std::thread::sleep(HELD);
    until("the shadow ended", || sink.len() == 1).await;
    let c = sink.taken().remove(0);
    assert!(c.queued >= HELD, "queued {:?}", c.queued);
    assert!(
        c.queued < HELD + RUN / 2,
        "queued {:?} counts the shadow's own run ({:?})",
        c.queued,
        c.shadow_latency
    );
    assert!(
        c.shadow_latency >= RUN,
        "shadow_latency {:?}",
        c.shadow_latency
    );
}
