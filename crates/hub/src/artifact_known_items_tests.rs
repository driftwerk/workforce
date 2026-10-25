//! The artifacts' known-item runs' rules (DW-676, PR 10), each written
//! before it.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use chrono::Duration as Span;
use workforce_agent::llm::provider::LlmProvider;
use workforce_common::settings::WorkforceConfig;
use workforce_common::shadow::{MemorySink, ShadowSettings};
use workforce_orchestrator::artifact_store::Artifact;

use super::*;
use crate::known_item_judge::tests::Answering;

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
}

fn now() -> DateTime<Utc> {
    day(22).and_hms_opt(3, 5, 0).unwrap().and_utc()
}

/// Artifact `n`: a title and content of words only it has, written a
/// week ago.
fn artifact(n: usize) -> ArtifactText {
    ArtifactText {
        id: format!("art-{n:04}"),
        title: format!("t{n}x0 t{n}x1"),
        content: (2..14)
            .map(|w| format!("t{n}x{w}"))
            .collect::<Vec<_>>()
            .join(" "),
        updated_at: now() - Span::days(7),
        pending: false,
    }
}

fn drawn(
    seed: u64,
    items: usize,
    artifacts: &[ArtifactText],
) -> (Vec<ArtifactItem>, KnownItemsDrawn) {
    let mut draw = ArtifactDraw::new(seed, items, now() - Span::minutes(30));
    for a in artifacts {
        draw.offer(a);
    }
    draw.finish()
}

/// The same seed and artifacts give the same items, whatever the order;
/// another day another set; what is too new or too short is counted, not
/// drawn; a query is a window of the artifact's title and content.
#[test]
fn artifacts_are_drawn_by_the_seed_and_what_cannot_be_is_counted() {
    let mut all: Vec<ArtifactText> = (0..200).map(artifact).collect();
    let seed = run_seed(3, day(20));
    let (items, counted) = drawn(seed, 40, &all);
    assert_eq!(
        (items.len(), counted.population, counted.drawn),
        (40, 200, 40)
    );
    all.reverse();
    assert_eq!(drawn(seed, 40, &all).0, items);
    let ids = |items: &[ArtifactItem]| items.iter().map(|i| i.id.clone()).collect::<BTreeSet<_>>();
    assert_ne!(ids(&drawn(run_seed(3, day(21)), 40, &all).0), ids(&items));
    for item in &items {
        let n: usize = item.id[4..].parse().unwrap();
        let a = artifact(n);
        let whole = format!("{} {}", a.title, a.content);
        assert!(
            whole.contains(&item.query) && item.query != whole,
            "{item:?}"
        );
    }

    let mut recent = artifact(1);
    recent.updated_at = now();
    let mut short = artifact(2);
    (short.title, short.content) = ("a b".into(), "c d e".into());
    let mut pending = artifact(3);
    pending.pending = true;
    let (items, counted) = drawn(seed, 10, &[artifact(0), recent, short, pending]);
    assert_eq!(
        items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ["art-0000"]
    );
    assert_eq!(
        counted.excluded,
        std::collections::BTreeMap::from([("pending", 1), ("too_recent", 1), ("too_short", 1)])
    );
}

/// The artifacts a run reads, and the items judged by day.
#[derive(Default)]
struct Held {
    artifacts: Vec<ArtifactText>,
    judged_before: i64,
    read_by_id: Mutex<Vec<String>>,
}

#[async_trait]
impl Artifacts for Held {
    async fn page(&self, after: Option<&str>, limit: usize) -> Result<Vec<ArtifactText>> {
        let mut all = self.artifacts.clone();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(all
            .into_iter()
            .filter(|a| after.is_none_or(|after| a.id.as_str() > after))
            .take(limit)
            .collect())
    }
    async fn by_id(&self, ids: &[String], chars: usize) -> Result<Vec<ArtifactText>> {
        self.read_by_id.lock().unwrap().extend(ids.iter().cloned());
        Ok(self
            .artifacts
            .iter()
            .filter(|a| ids.contains(&a.id))
            .map(|a| ArtifactText {
                content: a.content.chars().take(chars).collect(),
                ..a.clone()
            })
            .collect())
    }
    async fn judged_on(&self, _day: NaiveDate) -> Result<i64> {
        Ok(self.judged_before)
    }
}

type Finding = Box<dyn Fn(&str) -> Result<Vec<String>> + Send + Sync>;

/// An index that finds what `finding` says for the artifact a query was
/// made of, keeping the queries it was asked.
struct Finds {
    finding: Finding,
    complete: bool,
    asked: Mutex<Vec<(String, usize)>>,
}

impl Finds {
    fn new(finding: Finding) -> Arc<Self> {
        Arc::new(Self {
            finding,
            complete: true,
            asked: Mutex::new(Vec::new()),
        })
    }

    /// Itself first, then another.
    fn itself_first() -> Arc<Self> {
        Self::new(Box::new(|id| {
            Ok(vec![id.to_string(), "art-other".to_string()])
        }))
    }

    fn asked(&self) -> Vec<(String, usize)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl ArtifactIndexer for Finds {
    async fn remove(&self, _: String) -> Result<()> {
        unreachable!("a known-item run never writes")
    }
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        self.asked.lock().unwrap().push((query.to_string(), limit));
        let n = query
            .split_whitespace()
            .next()
            .and_then(|w| w.strip_prefix('t'))
            .and_then(|w| w.split('x').next())
            .unwrap_or_default();
        let id = format!("art-{:04}", n.parse::<usize>().unwrap_or(9999));
        Ok((self.finding)(&id)?
            .into_iter()
            .map(|id| (id, 0.5))
            .collect())
    }
    async fn reindex(&self, _: &Artifact, _: &str) -> Result<()> {
        unreachable!("a known-item run never writes")
    }
    async fn complete(&self) -> Result<bool> {
        Ok(self.complete)
    }
    async fn mark_incomplete(&self) -> Result<()> {
        unreachable!()
    }
    async fn mark_complete(&self) -> Result<()> {
        unreachable!()
    }
    async fn differing(&self, _: &[(String, String)]) -> Result<Vec<String>> {
        unreachable!()
    }
    async fn count(&self) -> Result<usize> {
        unreachable!()
    }
    async fn ids_after(
        &self,
        _: Option<String>,
        _: usize,
    ) -> Result<(Vec<String>, Option<String>)> {
        unreachable!()
    }
}

/// Keeps what a run records.
#[derive(Default)]
struct Kept {
    claims: Mutex<Vec<KnownItemClaim>>,
    items: Mutex<Vec<KnownItemRow>>,
    finished: Mutex<Option<KnownItemOutcome>>,
}

#[async_trait]
impl Recorder for Kept {
    async fn claim(&self, claim: &KnownItemClaim) -> Result<Option<i64>> {
        self.claims.lock().unwrap().push(claim.clone());
        Ok(Some(1))
    }
    async fn drawn(&self, _: i64, _: &KnownItemsDrawn) -> Result<()> {
        Ok(())
    }
    async fn items(&self, _: i64, items: &[KnownItemRow]) -> Result<()> {
        self.items.lock().unwrap().extend_from_slice(items);
        Ok(())
    }
    async fn finish(&self, _: i64, outcome: KnownItemOutcome, _: Option<&str>) -> Result<bool> {
        *self.finished.lock().unwrap() = Some(outcome);
        Ok(true)
    }
}

struct Rig {
    serving: Arc<Finds>,
    mirror: Arc<Finds>,
    kernel: Arc<Shadow>,
    in_step: Arc<AtomicBool>,
    held: Arc<Held>,
    kept: Arc<Kept>,
    /// The judge's LLM: how often it was looked up, and asked.
    looked_up: Arc<AtomicUsize>,
    llm: Arc<Answering>,
    runner: ArtifactRunner,
}

/// A rig of `n` artifacts, the judge as `judge` (TOML) says, and the LLM
/// there to be looked up, grading the first of each pool 2. (The others
/// found are no artifacts Postgres holds, so are not shown to it, and
/// gain nothing.)
fn rig(n: usize, judge: &str, judged_before: i64, serving: Arc<Finds>, mirror: Arc<Finds>) -> Rig {
    let kernel = Arc::new(
        Shadow::new(
            "artifacts",
            ShadowSettings {
                sample: 1.0,
                permits: 4,
                deadline: Duration::from_secs(5),
                trip_after: 3,
                rest: Duration::from_secs(30),
                k: 10,
                rbo_p: 0.9,
            },
            Arc::new(MemorySink::default()),
        )
        .unwrap(),
    );
    let in_step = Arc::new(AtomicBool::new(true));
    let held = Arc::new(Held {
        artifacts: (0..n).map(artifact).collect(),
        judged_before,
        ..Held::default()
    });
    let kept = Arc::new(Kept::default());
    let looked_up = Arc::new(AtomicUsize::new(0));
    let llm = Answering::new("1: 2");
    let config = WorkforceConfig::from_toml(judge).unwrap();
    let judge: JudgeSource = {
        let (looked_up, llm) = (Arc::clone(&looked_up), Arc::clone(&llm));
        Arc::new(move || {
            let (looked_up, llm) = (Arc::clone(&looked_up), Arc::clone(&llm));
            Judge::from_config(&config, move || {
                looked_up.fetch_add(1, Ordering::SeqCst);
                Some(llm as Arc<dyn LlmProvider>)
            })
        })
    };
    let runner = ArtifactRunner {
        source: held.clone(),
        serving: serving.clone(),
        mirror: mirror.clone(),
        kernel: {
            let kernel = Arc::clone(&kernel);
            Arc::new(move || Some(Arc::clone(&kernel)))
        },
        in_step: {
            let in_step = Arc::clone(&in_step);
            Arc::new(move || in_step.load(Ordering::SeqCst))
        },
        recorder: kept.clone(),
        judge,
        settings: KnownItemSettings {
            items: n,
            unscoped_items: 0,
            seed: 5,
            hour_utc: 2,
            until_hour_utc: 6,
            pace: Duration::ZERO,
        },
        rbo_p: 0.9,
        settle: SETTLE,
        permit_wait: Duration::from_secs(5),
        page_pace: Duration::ZERO,
    };
    Rig {
        serving,
        mirror,
        kernel,
        in_step,
        held,
        kept,
        looked_up,
        llm,
        runner,
    }
}

fn always() -> bool {
    true
}

/// Each artifact is asked of both indexes once, by its query, to depth
/// 10; where each put it is scored by id; the run is recorded under
/// `artifacts`, by meaning alone (alpha 1).
#[tokio::test]
async fn each_artifact_is_asked_of_both_indexes_and_scored_by_id() {
    let mirror = Finds::new(Box::new(|id| Ok(vec!["art-other".into(), id.to_string()])));
    let r = rig(15, "", 0, Finds::itself_first(), mirror);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!((ran.outcome, ran.items), (KnownItemOutcome::Ok, 15));
    let claim = r.kept.claims.lock().unwrap()[0].clone();
    assert_eq!(
        (
            claim.store.as_str(),
            claim.alpha_serving,
            claim.k,
            claim.derivation
        ),
        (STORE, 1.0, DEPTH, DERIVATION)
    );
    assert_eq!(r.serving.asked().len(), 15);
    assert_eq!(
        r.serving.asked(),
        r.mirror.asked(),
        "the two asked differently"
    );
    assert!(r.serving.asked().iter().all(|(_, depth)| *depth == DEPTH));
    let items = r.kept.items.lock().unwrap().clone();
    assert_eq!(items.len(), 15);
    for item in &items {
        assert_eq!((item.alpha, item.status), (1.0, KnownItemStatus::Asked));
        assert_eq!((item.primary.rank, item.shadow.rank), (Some(1), Some(2)));
        assert!(item.metrics.is_some());
        assert_eq!(item.ndcg, None, "judged while off");
    }
}

/// Off, the judge never runs: its LLM is not looked up nor asked, and no
/// artifact is read for it, through a whole run.
#[tokio::test]
async fn off_the_judge_never_runs_through_a_whole_run() {
    for off in ["", "[vellev.shadow.judge]\ndaily_cap = 500\n"] {
        let r = rig(12, off, 0, Finds::itself_first(), Finds::itself_first());
        r.runner
            .run(day(22), now(), &always)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r.looked_up.load(Ordering::SeqCst),
            0,
            "{off}: the LLM looked up"
        );
        assert_eq!(
            r.llm.calls.load(Ordering::SeqCst),
            0,
            "{off}: the LLM asked"
        );
        assert!(
            r.held.read_by_id.lock().unwrap().is_empty(),
            "{off}: artifacts read for it"
        );
        assert!(r
            .kept
            .items
            .lock()
            .unwrap()
            .iter()
            .all(|i| i.ndcg.is_none()));
    }
}

/// On, the judge grades the pool of both sides' first five for as many
/// items as the day's cap leaves, counting those judged in the day's
/// earlier runs, and each side's nDCG@5 is recorded.
#[tokio::test]
async fn on_the_judge_grades_up_to_the_days_cap() {
    let r = rig(
        10,
        "[vellev.shadow.judge]\nenabled = true\ndaily_cap = 4\n",
        1,
        Finds::itself_first(),
        Finds::new(Box::new(|id| {
            Ok(vec!["art-other".into(), "art-else".into(), id.to_string()])
        })),
    );
    r.runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    let items = r.kept.items.lock().unwrap().clone();
    let judged: Vec<&KnownItemRow> = items.iter().filter(|i| i.ndcg.is_some()).collect();
    assert_eq!(
        judged.len(),
        3,
        "the cap of 4, one judged earlier in the day"
    );
    assert_eq!(r.llm.calls.load(Ordering::SeqCst), 3);
    for item in judged {
        // The pool is the artifact, graded 2, and two Postgres does not hold.
        let (primary, shadow) = item.ndcg.unwrap();
        assert!((primary - 1.0).abs() < 1e-9, "{primary}");
        assert!((shadow - 0.5).abs() < 1e-9, "{shadow}");
    }
}

/// No run is claimed while the mirror is out of step, the serving index
/// incomplete, the shadow paused, or the hub without the artifact shadow.
#[tokio::test]
async fn no_run_while_the_two_cannot_be_compared() {
    let r = rig(5, "", 0, Finds::itself_first(), Finds::itself_first());
    r.in_step.store(false, Ordering::SeqCst);
    assert_eq!(r.runner.run(day(22), now(), &always).await.unwrap(), None);

    let incomplete = Arc::new(Finds {
        finding: Box::new(|id| Ok(vec![id.to_string()])),
        complete: false,
        asked: Mutex::new(Vec::new()),
    });
    let r2 = rig(5, "", 0, incomplete, Finds::itself_first());
    assert_eq!(r2.runner.run(day(22), now(), &always).await.unwrap(), None);

    let r3 = rig(5, "", 0, Finds::itself_first(), Finds::itself_first());
    r3.kernel.set_enabled(false);
    assert_eq!(r3.runner.run(day(22), now(), &always).await.unwrap(), None);

    let mut r4 = rig(5, "", 0, Finds::itself_first(), Finds::itself_first());
    r4.runner.kernel = Arc::new(|| None);
    assert_eq!(r4.runner.run(day(22), now(), &always).await.unwrap(), None);

    for r in [&r, &r2, &r3, &r4] {
        assert!(r.kept.claims.lock().unwrap().is_empty());
        assert!(r.serving.asked().is_empty() && r.mirror.asked().is_empty());
    }
}

/// The mirror failing is recorded as failing, never as a miss.
#[tokio::test]
async fn a_mirror_that_fails_is_recorded_as_failing() {
    let failing = Finds::new(Box::new(|_| Err(eyre::eyre!("vellev answered 503"))));
    let r = rig(5, "", 0, Finds::itself_first(), failing);
    let ran = r
        .runner
        .run(day(22), now(), &always)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ran.outcome, KnownItemOutcome::Ok);
    for item in r.kept.items.lock().unwrap().iter() {
        assert_eq!(item.primary.rank, Some(1));
        assert_eq!(item.shadow.rank, None);
        assert!(item
            .shadow
            .error
            .as_deref()
            .is_some_and(|e| e.contains("503")));
    }
}

/// Twelve artifacts, each on a topic of its own.
const TOPICS: [(&str, &str); 12] = [
    ("Rolling back a release", "how to roll back a bad deploy of the hub to the version before it, step by step"),
    ("Sourdough bread", "feed the starter the night before, then mix flour, water and salt and leave the dough to rise"),
    ("Otters in the river", "river otters hold paws while they sleep so the current does not pull them apart"),
    ("Quarterly tax return", "the quarterly tax return is filed online before the end of the month after the quarter"),
    ("Tuning the bicycle gears", "turn the barrel adjuster a quarter turn until the chain runs quietly on every cog"),
    ("Rotating database passwords", "rotate the database password, update the vault entry, then restart each service that reads it"),
    ("Planting tomatoes", "plant tomato seedlings deep in warm soil after the last frost and water them every morning"),
    ("Chess openings", "the queen's gambit offers a pawn early to take control of the centre of the board"),
    ("Booking the team offsite", "book the team offsite venue three months ahead and ask everyone about dietary needs"),
    ("Replacing a car tyre", "loosen the wheel nuts, jack up the car, swap the tyre and tighten the nuts in a star"),
    ("Learning Spanish verbs", "practise the present tense of regular Spanish verbs ending in ar, er and ir every day"),
    ("Brewing green tea", "brew green tea leaves at about eighty degrees for two minutes so it does not turn bitter"),
];

/// Against Postgres and a node that runs the local model: artifacts drawn
/// from Postgres, asked of vellev (both sides here, as no Weaviate here
/// embeds), each come back first, and the run is summed as such.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_against_postgres_and_the_node_finds_each_artifact_first() {
    let Some(node) = workforce_vellev::testing::Node::embedding().await else {
        return;
    };
    workforce_postgres::test_support::with_test_pool(
        workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool,
        |pool| async move {
            use crate::artifact_index::{ArtifactIndex, ArtifactVectors, VellevArtifacts};
            let store = ShadowStore::with_pool(pool.clone()).await.unwrap();
            let vellev = VellevArtifacts::new(&node.url, &node.key, &node.tenant).unwrap();
            vellev.ensure_collection().await.unwrap();
            let conn = pool.get().await.unwrap();
            for (n, (title, content)) in TOPICS.iter().enumerate() {
                let id = format!("art-{n:04}");
                conn.execute(
                    "INSERT INTO artifacts (id, artifact_type, title, content, owner_id,
                         created_by, last_modified_by, created_at, updated_at)
                     VALUES ($1, 'document', $2, $3, 'o', 'o', 'o', now() - interval '1 day',
                         now() - interval '1 day')",
                    &[&id, title, content],
                )
                .await
                .unwrap();
                vellev
                    .upsert(&id, &format!("{title}\n\n{content}"), "h")
                    .await
                    .unwrap();
            }
            // Taken by both indexes, as their keepers would (each write
            // noted them); still noted, none would be drawn.
            conn.batch_execute("DELETE FROM artifact_index_pending")
                .await
                .unwrap();
            drop(conn);
            // As a backfill that finished marks it: searched by meaning.
            vellev.mark_incomplete().await.unwrap();
            vellev.mark_complete().await.unwrap();
            let kernel = Arc::new(
                Shadow::new(
                    "artifacts",
                    ShadowSettings {
                        sample: 1.0,
                        permits: 4,
                        deadline: Duration::from_secs(20),
                        trip_after: 3,
                        rest: Duration::from_secs(60),
                        k: 10,
                        rbo_p: 0.9,
                    },
                    Arc::new(MemorySink::default()),
                )
                .unwrap(),
            );
            let index = || {
                Arc::new(ArtifactIndex::new(
                    VellevArtifacts::new(&node.url, &node.key, &node.tenant).unwrap(),
                )) as Arc<dyn ArtifactIndexer>
            };
            let runner = ArtifactRunner {
                source: store.clone(),
                serving: index(),
                mirror: index(),
                kernel: Arc::new(move || Some(Arc::clone(&kernel))),
                in_step: Arc::new(|| true),
                recorder: store.clone(),
                judge: Arc::new(|| Ok(None)),
                settings: KnownItemSettings {
                    items: 12,
                    unscoped_items: 0,
                    seed: 2,
                    hour_utc: 2,
                    until_hour_utc: 6,
                    pace: Duration::ZERO,
                },
                rbo_p: 0.9,
                settle: SETTLE,
                permit_wait: Duration::from_secs(5),
                page_pace: Duration::ZERO,
            };
            let at = Utc::now();
            let ran = runner
                .run(at.date_naive(), at, &always)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (ran.outcome, ran.drawn.drawn, ran.items),
                (KnownItemOutcome::Ok, 12, 12),
                "{ran:?}"
            );
            let summary = store.known_item_summary(ran.run).await.unwrap();
            let s = &summary[0];
            assert_eq!(
                (
                    s.alpha,
                    s.both_answered,
                    s.primary_first,
                    s.shadow_first,
                    s.shadow_within_5
                ),
                (1.0, 12, 12, 12, 12),
                "{s:?}"
            );
        },
    )
    .await;
}
