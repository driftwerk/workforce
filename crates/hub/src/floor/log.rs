use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};
use workforce_common::channels::desk::DeskSignal;
use workforce_common::settings::WorkforceConfig;
use workforce_desk::floor::{Scope, Turn, TurnLog, TurnOp};
use workforce_postgres::{PostgresConfig, PostgresError};

use super::gists::Gists;
use super::plans::Plans;
use super::postgres::PgFloorStore;
use super::signal::Signals;
use crate::hub_core::logins::plan_usage::observe_with;

/// How often the log is pruned.
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a turn and a reading are kept, unless the turn still asks the
/// person something.
const KEPT_DAYS: i64 = 8;
/// Ops waiting for the writer before new ones are dropped.
const BACKLOG: usize = 1024;
/// How often a full backlog is reported.
const DROP_WARN_EVERY: Duration = Duration::from_secs(60);
/// How many times an op is tried while what stops it is passing.
const TRIES: u32 = 3;
/// The wait before an op is tried again, doubled each time.
const RETRY_FIRST: Duration = Duration::from_millis(200);

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// The floor's store as the hub runs it: the log written through its one
/// writer, the plan readings kept beside it, the gists written as the log
/// is, the store they all read from, and the signals that tell a person's
/// links what changed.
#[derive(Clone)]
pub struct Installed {
    pub log: Arc<dyn TurnLog>,
    pub plans: Arc<Plans>,
    pub gists: Gists,
    pub store: Arc<PgFloorStore>,
    pub signals: Signals,
}

impl Installed {
    /// Starts the writer over `store`.
    pub fn open(store: PgFloorStore) -> Self {
        let store = Arc::new(store);
        let signals = Signals::start();
        let told = signals.clone();
        let gists = Gists::start(
            Arc::clone(&store) as Arc<dyn super::gists::Kept>,
            Arc::new(move |owner| told.send(DeskSignal::Floor, owner)),
        );
        Self {
            log: Arc::new(Logged::start(
                Arc::clone(&store),
                signals.clone(),
                gists.clone(),
            )),
            plans: Plans::new(Arc::clone(&store), signals.clone()),
            gists,
            store,
            signals,
        }
    }
}

/// The installed store, once [`install`] opened it.
pub fn installed() -> Option<&'static Installed> {
    INSTALLED.get()
}

/// The installed log.
pub fn turns() -> Option<&'static Arc<dyn TurnLog>> {
    installed().map(|i| &i.log)
}

/// The first wait before a store that couldn't be opened is tried again.
const REOPEN_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between tries.
const REOPEN_MOST: Duration = Duration::from_secs(60);

pub(super) async fn install(config: &WorkforceConfig) {
    static OPENED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    // A config without Postgres opens nothing and settles nothing: the
    // floor stays the first one's with Postgres to open (in one process,
    // a hub composed without it never keeps a later one from its store).
    let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) else {
        return;
    };
    // Opened once. The hub installs once, at boot: a store that couldn't
    // be opened then (Postgres down) is tried again in the background
    // until it opens.
    OPENED
        .get_or_init(|| async {
            let Err(e) = open(pg).await else {
                return;
            };
            warn!(
                target: "floor",
                error = %format!("{e:#}"),
                "floor store unavailable; turns are not recorded until it opens"
            );
            let pg = pg.clone();
            tokio::spawn(async move {
                until_opened(|| open(&pg), REOPEN_FIRST, REOPEN_MOST).await;
                info!(target: "floor", "floor store opened; turns are recorded");
            });
        })
        .await;
}

/// Opens the store over `pg` and installs it.
async fn open(pg: &PostgresConfig) -> eyre::Result<()> {
    let store = PgFloorStore::new(pg).await?;
    let installed = Installed::open(store);
    tokio::spawn(prune_forever(Arc::clone(&installed.store)));
    observe_with(installed.plans.clone());
    let _ = INSTALLED.set(installed);
    Ok(())
}

/// Tries `open` after `wait`, doubling the wait up to `most`, until it
/// succeeds.
async fn until_opened<F, Fut>(mut open: F, mut wait: Duration, most: Duration)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = eyre::Result<()>>,
{
    loop {
        tokio::time::sleep(wait).await;
        if open().await.is_ok() {
            return;
        }
        wait = (wait * 2).min(most);
    }
}

/// The log as the hub writes it: every op through one writer, in order, the
/// owner of each that changes what the floor lists told, and each shown to
/// the gists. A turn's correctness never waits on it; a full backlog drops
/// the op.
struct Logged {
    tx: mpsc::Sender<TurnOp>,
    store: Arc<PgFloorStore>,
    warned: Mutex<Option<Instant>>,
}

impl Logged {
    fn start(store: Arc<PgFloorStore>, signals: Signals, gists: Gists) -> Self {
        let (tx, mut rx) = mpsc::channel::<TurnOp>(BACKLOG);
        let writer = Arc::clone(&store);
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                match applied(|| writer.apply(&op), RETRY_FIRST).await {
                    Ok(Some(owner)) => {
                        gists.saw(&op);
                        if op.lists() {
                            signals.send(DeskSignal::Floor, owner.as_deref())
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!(
                        target: "floor",
                        op = op.kind(),
                        turn = ?op.turn(),
                        error = %format!("{e:#}"),
                        "recording a turn failed; dropped"
                    ),
                }
            }
        });
        Self {
            tx,
            store,
            warned: Mutex::new(None),
        }
    }
}

/// What `apply` answered once it succeeded, or failed for good, or failed
/// [`TRIES`] times over something passing, waiting `wait`, then twice that,
/// between tries.
async fn applied<T, F, Fut>(mut apply: F, mut wait: Duration) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = eyre::Result<T>>,
{
    let mut tried = 1;
    loop {
        match apply().await {
            Err(e) if tried < TRIES && passing(&e) => {
                tokio::time::sleep(wait).await;
                wait *= 2;
                tried += 1;
            }
            answered => return answered,
        }
    }
}

/// Whether `e` is one a moment can clear: no connection to be had or kept,
/// or Postgres giving up on a lock, a serialization, a deadlock or its own
/// load.
fn passing(e: &eyre::Report) -> bool {
    e.chain().any(|cause| {
        cause.is::<std::io::Error>()
            || cause.downcast_ref::<PostgresError>().is_some_and(|pg| {
                matches!(
                    pg,
                    PostgresError::Pool(_) | PostgresError::LockNotAvailable(_)
                )
            })
            || cause
                .downcast_ref::<tokio_postgres::Error>()
                .is_some_and(|pg| {
                    pg.is_closed()
                        || pg.code().is_some_and(|state| {
                            let code = state.code();
                            ["08", "40001", "40P01", "53300", "55P03", "57P01", "57P03"]
                                .iter()
                                .any(|passing| code.starts_with(passing))
                        })
                })
    })
}

#[async_trait]
impl TurnLog for Logged {
    fn send(&self, op: TurnOp) {
        if self.tx.try_send(op).is_err() {
            let mut warned = self.warned.lock();
            if warned.is_none_or(|at| at.elapsed() >= DROP_WARN_EVERY) {
                *warned = Some(Instant::now());
                warn!(target: "floor", "the turn log is behind; dropping what it can't keep up with");
            }
        }
    }

    async fn relevant(&self, scope: &Scope, since: DateTime<Utc>) -> eyre::Result<Vec<Turn>> {
        self.store.relevant(scope, since).await
    }
}

async fn prune_forever(store: Arc<PgFloorStore>) {
    let mut tick = tokio::time::interval(PRUNE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let before = Utc::now() - chrono::Duration::days(KEPT_DAYS);
        match store.prune(before).await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(target: "floor", removed, "pruned the turn log"),
            Err(e) => {
                warn!(target: "floor", error = %format!("{e:#}"), "pruning the turn log failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_store_that_wont_open_is_tried_again_waiting_longer_each_time_up_to_a_minute() {
        let tries = Arc::new(AtomicUsize::new(0));
        let began = tokio::time::Instant::now();
        let counted = Arc::clone(&tries);
        until_opened(
            move || {
                let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if n < 9 {
                        Err(eyre::eyre!("Postgres is down"))
                    } else {
                        Ok(())
                    }
                }
            },
            REOPEN_FIRST,
            REOPEN_MOST,
        )
        .await;
        assert_eq!(
            tries.load(Ordering::SeqCst),
            9,
            "tried until it opened, then no more"
        );
        // 1 + 2 + 4 + 8 + 16 + 32, then a minute each.
        assert_eq!(
            began.elapsed(),
            Duration::from_secs(1 + 2 + 4 + 8 + 16 + 32 + 60 + 60 + 60)
        );
    }

    /// Each try of an op answering `answers` in turn: what [`applied`]
    /// answered, how many times it tried, and how long it took.
    async fn tried(answers: Vec<eyre::Result<u8>>) -> (eyre::Result<u8>, usize, Duration) {
        let answers = Mutex::new(answers.into_iter());
        let tries = AtomicUsize::new(0);
        let began = tokio::time::Instant::now();
        let answered = applied(
            || {
                tries.fetch_add(1, Ordering::SeqCst);
                let next = answers.lock().next().expect("tried past its answers");
                async move { next }
            },
            RETRY_FIRST,
        )
        .await;
        (answered, tries.into_inner(), began.elapsed())
    }

    fn pool_timeout() -> eyre::Result<u8> {
        Err(PostgresError::Pool("timed out waiting for a connection".into()).into())
    }

    #[tokio::test(start_paused = true)]
    async fn an_op_a_passing_error_stopped_is_tried_again_and_one_that_cant_be_never() {
        let (answered, tries, took) = tried(vec![pool_timeout(), pool_timeout(), Ok(7)]).await;
        assert_eq!(answered.unwrap(), 7);
        assert_eq!((tries, took), (3, RETRY_FIRST * 3));

        let (answered, tries, took) =
            tried(vec![pool_timeout(), pool_timeout(), pool_timeout()]).await;
        assert!(passing(&answered.unwrap_err()));
        assert_eq!(
            (tries, took),
            (TRIES as usize, RETRY_FIRST * 3),
            "then dropped"
        );

        let (answered, tries, took) = tried(vec![Err(PostgresError::Query(
            "no such column".into(),
        )
        .into())])
        .await;
        assert!(!passing(&answered.unwrap_err()));
        assert_eq!((tries, took), (1, Duration::ZERO), "dropped at once");

        let lost = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(passing(
            &eyre::Report::new(lost).wrap_err("writing the turn")
        ));
    }
}
