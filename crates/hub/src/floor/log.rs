use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::warn;
use workforce_common::channels::desk::DeskSignal;
use workforce_common::settings::WorkforceConfig;
use workforce_desk::floor::{Scope, Turn, TurnLog, TurnOp};

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

pub(super) async fn install(config: &WorkforceConfig) {
    static OPENED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    OPENED
        .get_or_init(|| async {
            let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) else {
                return;
            };
            match PgFloorStore::new(pg).await {
                Ok(store) => {
                    let installed = Installed::open(store);
                    tokio::spawn(prune_forever(Arc::clone(&installed.store)));
                    observe_with(installed.plans.clone());
                    let _ = INSTALLED.set(installed);
                }
                Err(e) => warn!(
                    target: "floor",
                    error = %format!("{e:#}"),
                    "floor store unavailable; turns are not recorded"
                ),
            }
        })
        .await;
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
                match writer.apply(&op).await {
                    Ok(Some(owner)) => {
                        gists.saw(&op);
                        if op.lists() {
                            signals.send(DeskSignal::Floor, owner.as_deref())
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(target: "floor", error = %format!("{e:#}"), "recording a turn failed")
                    }
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
