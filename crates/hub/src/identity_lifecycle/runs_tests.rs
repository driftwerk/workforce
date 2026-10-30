use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use workforce_identity::lifecycle::{LifecycleOpStore, OpStatus};
use workforce_identity::store::IdentityStore;

use super::runs::{Operation, Steps};
use super::tests::{hub, Hub};
use super::*;

/// Visits every category doing nothing, except at `at`.
struct Gated {
    at: Category,
    then: At,
}

enum At {
    /// Say it got there, then wait to be let go.
    Wait {
        reached: Arc<Notify>,
        release: Arc<Notify>,
    },
    /// Begin the hub's shutdown.
    Stop(CancellationToken),
}

#[async_trait]
impl Steps for Gated {
    async fn step(
        &self,
        _: &Context<'_>,
        category: Category,
        _: &Person,
        _: bool,
    ) -> Result<Tally> {
        if category == self.at {
            match &self.then {
                At::Wait { reached, release } => {
                    reached.notify_one();
                    release.notified().await;
                }
                At::Stop(stop) => stop.cancel(),
            }
        }
        Ok(Tally::default())
    }
}

/// The same hub's stores, run by another replica whose runs `runs` owns.
fn replica(hub: &Hub, runs: Runs) -> Arc<Lifecycle> {
    let lifecycle = &hub.lifecycle;
    Arc::new(Lifecycle {
        registry: Arc::clone(&lifecycle.registry),
        identities: Arc::clone(&lifecycle.identities),
        devices: None,
        channels: None,
        defs: None,
        artifacts: None,
        pictures: None,
        external_skills: Arc::clone(&lifecycle.external_skills),
        data_dir: lifecycle.data_dir.clone(),
        runs,
    })
}

fn gated(who: &Identity, actor: &Actor, at: Category, then: At) -> Operation {
    Operation {
        kind: "delete",
        subject: Subject::Identity,
        who: Person::of(who),
        target: None,
        steps: Box::new(Gated { at, then }),
        actor: actor.clone(),
    }
}

/// The operation's record once it is no longer running.
async fn settled(lifecycle: &Lifecycle, id: Uuid) -> workforce_identity::lifecycle::LifecycleOp {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let op = lifecycle.operation(id).unwrap();
            if op.status != OpStatus::Running {
                return op;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the operation settles")
}

/// A caller that gives up once a run has started, as a browser or a proxy
/// does at its timeout, leaves the run going to the end.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_run_carries_on_after_the_caller_that_started_it_is_gone() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();
    let (reached, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let operation = gated(
        &alice,
        &admin,
        Category::Sessions,
        At::Wait {
            reached: Arc::clone(&reached),
            release: Arc::clone(&release),
        },
    );
    let lifecycle = Arc::clone(&hub.lifecycle);
    let (claimed, started) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        let _ = claimed.send(lifecycle.start(operation));
        std::future::pending::<()>().await;
    });
    let op = tokio::time::timeout(std::time::Duration::from_secs(10), started)
        .await
        .expect("the start answers before the run ends")
        .unwrap()
        .unwrap();
    reached.notified().await;
    caller.abort();
    let _ = caller.await;
    release.notify_one();

    let done = settled(&hub.lifecycle, op.id).await;

    assert_eq!(done.status, OpStatus::Complete, "{:#}", done.report);
    assert_eq!(done.report["complete"], true);
    assert!(done.report.get("progress").is_none());
    assert_eq!(
        done.report["categories"].as_array().unwrap().len(),
        Category::ALL.len()
    );
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while hub.lifecycle.runs.running() > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the task ends once it has recorded how it ended");
}

/// A real run answers at once with what it claimed; a dry run answers with
/// its report and records nothing; a subject under way is refused before
/// anything starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_answers_at_once_and_a_subject_under_way_is_refused() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let merge = || Request::Merge {
        from: alice.id,
        into: bob.id,
    };

    let preview = hub
        .lifecycle
        .launch(merge(), admin.clone(), true)
        .await
        .unwrap();
    assert!(matches!(preview, Launched::Report(ref report) if report.dry_run));
    assert!(hub
        .lifecycle
        .operations(Some(alice.id), 10)
        .unwrap()
        .is_empty());

    let mut held = super::tests::running_elsewhere(alice.id);
    assert!(hub.identities.claim_op(&held, Utc::now()).unwrap());
    let refused = hub.lifecycle.launch(merge(), admin.clone(), false).await;
    assert!(
        matches!(refused, Err(LifecycleError::Busy(_))),
        "{refused:?}"
    );
    assert_eq!(hub.lifecycle.runs.running(), 0, "nothing started");

    held.lease_until = Utc::now();
    hub.identities
        .renew_op(held.id, held.lease_until, &held.report)
        .unwrap();
    let Launched::Started(op) = hub.lifecycle.launch(merge(), admin, false).await.unwrap() else {
        panic!("a real run starts");
    };
    assert_eq!(
        (op.kind.as_str(), op.subject, op.target),
        ("merge", alice.id, Some(bob.id))
    );
    assert_eq!(
        settled(&hub.lifecycle, op.id).await.status,
        OpStatus::Complete
    );
}

/// While a category runs, the operation's record says which, and what the
/// categories before it did.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_operation_records_its_progress() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();
    let (reached, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let operation = gated(
        &alice,
        &admin,
        Category::Sessions,
        At::Wait {
            reached: Arc::clone(&reached),
            release: Arc::clone(&release),
        },
    );
    let op = hub.lifecycle.claim(&operation).unwrap();
    let lifecycle = Arc::clone(&hub.lifecycle);
    let claimed = op.clone();
    let run = tokio::spawn(async move { lifecycle.run(&operation, Some(&claimed)).await });

    reached.notified().await;
    let under_way = hub.lifecycle.operation(op.id).unwrap();
    let at = Category::ALL
        .iter()
        .position(|c| *c == Category::Sessions)
        .unwrap();
    assert_eq!(under_way.status, OpStatus::Running);
    assert_eq!(
        under_way.report["progress"],
        serde_json::json!({"category": "sessions", "done": at, "total": Category::ALL.len()})
    );
    assert_eq!(under_way.report["categories"].as_array().unwrap().len(), at);
    release.notify_one();

    let report = run.await.unwrap().unwrap();
    assert!(report.complete && report.progress.is_none());
    assert_eq!(
        hub.lifecycle.operation(op.id).unwrap().status,
        OpStatus::Complete
    );
}

/// Once the hub begins to shut down, a run stops before its next category
/// and records why, keeping the subject; a run on the successor finishes.
#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_stops_a_run_between_categories_and_a_rerun_finishes() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let stop = CancellationToken::new();
    let stopping = replica(&hub, Runs::until(stop.clone()));
    let operation = gated(&alice, &admin, Category::Teams, At::Stop(stop));
    let op = stopping.claim(&operation).unwrap();

    let report = stopping.run(&operation, Some(&op)).await.unwrap();

    let at = Category::ALL
        .iter()
        .position(|c| *c == Category::Teams)
        .unwrap();
    assert!(!report.complete);
    assert_eq!(
        report.categories.len(),
        at + 1,
        "the category under way finished"
    );
    assert!(report.error.is_some());
    let recorded = stopping.operation(op.id).unwrap();
    assert_eq!(recorded.status, OpStatus::Incomplete);
    assert!(recorded.lease_until <= Utc::now(), "the lease is let go");
    assert!(hub.identities.get_by_id(alice.id).unwrap().is_some());
    let refused = stopping
        .launch(
            Request::Merge {
                from: alice.id,
                into: bob.id,
            },
            admin.clone(),
            false,
        )
        .await;
    assert!(
        matches!(refused, Err(LifecycleError::Busy(_))),
        "{refused:?}"
    );

    let rerun = hub
        .lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();
    assert!(rerun.complete, "{rerun:#?}");
    assert!(hub.identities.get_by_id(alice.id).unwrap().is_none());
}

/// Following a person's standing runs through the same runner as a merge
/// and a delete, recorded and leased, and never removes the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_suspension_runs_to_the_end_and_never_removes_the_person() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();

    for report in [
        hub.lifecycle.suspend(alice.id, &admin).await.unwrap(),
        hub.lifecycle.resume(alice.id, &admin).await.unwrap(),
    ] {
        assert!(report.complete, "{report:#?}");
        assert!(report.progress.is_none());
        assert!(!report.categories.is_empty());
        assert!(report
            .categories
            .iter()
            .all(|c| c.rules.suspend != SuspendRule::Unaffected));
    }

    let alice = hub.identities.get_by_id(alice.id).unwrap().expect("kept");
    assert_eq!(alice.status, IdentityStatus::Active);
    let ops = hub.lifecycle.operations(Some(alice.id), 10).unwrap();
    assert_eq!(
        ops.iter()
            .map(|op| (op.kind.as_str(), op.status))
            .collect::<Vec<_>>(),
        [
            ("resume", OpStatus::Complete),
            ("suspend", OpStatus::Complete)
        ]
    );
}

/// A category under way renews the lease at every beat, so a long one is
/// never taken over.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_category_renews_its_lease_while_it_runs() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();
    let beating = replica(
        &hub,
        Runs::default().beating_every(std::time::Duration::from_millis(20)),
    );
    let (reached, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let operation = gated(
        &alice,
        &admin,
        Category::Sessions,
        At::Wait {
            reached: Arc::clone(&reached),
            release: Arc::clone(&release),
        },
    );
    let op = beating.claim(&operation).unwrap();
    let run = tokio::spawn({
        let (beating, op) = (Arc::clone(&beating), op.clone());
        async move { beating.run(&operation, Some(&op)).await }
    });
    reached.notified().await;
    let lapsing = Utc::now() + Duration::seconds(1);
    let progress = beating.operation(op.id).unwrap().report;
    assert!(hub.identities.renew_op(op.id, lapsing, &progress).unwrap());

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while beating.operation(op.id).unwrap().lease_until <= lapsing {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a beat renews the lease");
    release.notify_one();

    assert!(run.await.unwrap().unwrap().complete);
}

/// Once another run has taken the operation over, the next beat drops the
/// category under way and the run records nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn a_beat_that_finds_the_operation_taken_over_stops_the_run() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();
    let beating = replica(
        &hub,
        Runs::default().beating_every(std::time::Duration::from_millis(20)),
    );
    let reached = Arc::new(Notify::new());
    let operation = gated(
        &alice,
        &admin,
        Category::Sessions,
        At::Wait {
            reached: Arc::clone(&reached),
            release: Arc::new(Notify::new()),
        },
    );
    let op = beating.claim(&operation).unwrap();
    let run = tokio::spawn({
        let (beating, op) = (Arc::clone(&beating), op.clone());
        async move { beating.run(&operation, Some(&op)).await }
    });
    reached.notified().await;
    let taken_over = serde_json::json!({"taken": "over"});
    assert!(hub
        .identities
        .finish_op(op.id, OpStatus::Interrupted, &taken_over, Utc::now())
        .unwrap());

    let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("the run stops though its category never finishes")
        .unwrap();

    assert!(
        matches!(stopped, Err(LifecycleError::Busy(_))),
        "{stopped:?}"
    );
    let recorded = beating.operation(op.id).unwrap();
    assert_eq!(
        (recorded.status, recorded.report),
        (OpStatus::Interrupted, taken_over)
    );
    assert!(hub.identities.get_by_id(alice.id).unwrap().is_some());
}
