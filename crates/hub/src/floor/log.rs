use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::warn;
use workforce_common::settings::WorkforceConfig;
use workforce_desk::floor::{Scope, Turn, TurnLog, TurnOp};

use super::plans::Plans;
use super::postgres::PgFloorStore;
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
/// writer, the plan readings kept beside it, and the store both read from.
#[derive(Clone)]
pub struct Installed {
    pub log: Arc<dyn TurnLog>,
    pub plans: Arc<Plans>,
    pub store: Arc<PgFloorStore>,
}

impl Installed {
    /// Starts the writer over `store`.
    pub fn open(store: PgFloorStore) -> Self {
        let store = Arc::new(store);
        Self {
            log: Arc::new(Logged::start(Arc::clone(&store))),
            plans: Plans::new(Arc::clone(&store)),
            store,
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

/// The log as the hub writes it: every op through one writer, in order.
/// A turn's correctness never waits on it; a full backlog drops the op.
struct Logged {
    tx: mpsc::Sender<TurnOp>,
    store: Arc<PgFloorStore>,
    warned: Mutex<Option<Instant>>,
}

impl Logged {
    fn start(store: Arc<PgFloorStore>) -> Self {
        let (tx, mut rx) = mpsc::channel::<TurnOp>(BACKLOG);
        let writer = Arc::clone(&store);
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                if let Err(e) = writer.apply(&op).await {
                    warn!(target: "floor", error = %format!("{e:#}"), "recording a turn failed");
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
