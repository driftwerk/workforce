//! The language servers a hub keeps running: one per project and command,
//! reused by every call that needs it, at most a configured number at once,
//! and shut down when idle. A server answers one question at a time; a
//! call that finds its server busy waits for it.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::OwnedMutexGuard;
use tracing::info;

use super::server::{LanguageServer, Launcher, LspError, ServerSpec, Timeouts};

/// How often idle and dead servers are looked for.
const REAP_EVERY: Duration = Duration::from_secs(30);

struct Slot {
    server: Arc<tokio::sync::Mutex<Option<LanguageServer>>>,
    last_used: Mutex<Instant>,
}

impl Slot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            server: Arc::new(tokio::sync::Mutex::new(None)),
            last_used: Mutex::new(Instant::now()),
        })
    }
}

pub struct Pool {
    slots: Mutex<HashMap<ServerSpec, Arc<Slot>>>,
    max: usize,
    idle: Duration,
    reaper: OnceLock<()>,
}

/// One call's hold on its server.
pub struct Lease {
    slot: Arc<Slot>,
    guard: OwnedMutexGuard<Option<LanguageServer>>,
}

impl Lease {
    pub fn server(&mut self) -> &mut LanguageServer {
        self.guard
            .as_mut()
            .expect("a lease is only handed out over a running server")
    }

    /// Stop the server: it failed in a way that makes its next answer
    /// untrustworthy.
    pub fn discard(mut self) {
        if let Some(server) = self.guard.take() {
            tokio::spawn(server.shutdown());
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        *self.slot.last_used.lock() = Instant::now();
    }
}

static SHARED: OnceLock<Arc<Pool>> = OnceLock::new();

impl Pool {
    pub fn new(max: usize, idle: Duration) -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(HashMap::new()),
            max: max.max(1),
            idle,
            reaper: OnceLock::new(),
        })
    }

    /// The hub's pool, made with the first limits asked for: every agent
    /// shares the install's.
    pub fn shared(max: usize, idle: Duration) -> Arc<Self> {
        Arc::clone(SHARED.get_or_init(|| Self::new(max, idle)))
    }

    /// The running server for `spec`, started if there is none. Starting one
    /// past the limit first stops the longest idle; with none idle, the call
    /// is refused. A server busy with another call is waited for, as long as
    /// that call may itself take.
    pub async fn checkout(
        self: &Arc<Self>,
        spec: ServerSpec,
        launcher: &dyn Launcher,
        timeouts: Timeouts,
        max_memory_mb: u64,
    ) -> Result<Lease, LspError> {
        self.start_reaper();
        let deadline = tokio::time::Instant::now() + timeouts.startup + timeouts.request;
        loop {
            let (slot, evicted) = self.slot_for(&spec)?;
            if let Some(server) = evicted {
                server.shutdown().await;
            }
            let waited = Arc::clone(&slot.server).lock_owned();
            let Ok(mut guard) = tokio::time::timeout_at(deadline, waited).await else {
                return Err(LspError::NotReady(format!(
                    "{} for {} is busy with another call; try again shortly",
                    spec.kind.name(),
                    spec.root.display()
                )));
            };
            // Reaped, evicted or forgotten while this call waited: a server
            // started in it would run outside the limit.
            if !self.holds(&spec, &slot) {
                continue;
            }
            if guard.as_ref().is_some_and(|server| !server.is_alive()) {
                *guard = None;
            }
            if guard.is_none() {
                match LanguageServer::start(spec.clone(), launcher, timeouts, max_memory_mb).await {
                    Ok(server) => {
                        info!(server = spec.kind.name(), root = %spec.root.display(), "language server started");
                        *guard = Some(server);
                    }
                    Err(e) => {
                        self.forget(&spec, &slot);
                        return Err(e);
                    }
                }
            }
            return Ok(Lease { slot, guard });
        }
    }

    /// The slot for `spec`, and the idle server stopped to make room for it.
    fn slot_for(&self, spec: &ServerSpec) -> Result<(Arc<Slot>, Option<LanguageServer>), LspError> {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get(spec) {
            return Ok((Arc::clone(slot), None));
        }
        let mut evicted = None;
        if slots.len() >= self.max {
            let idle = slots
                .iter()
                .filter_map(|(key, slot)| {
                    let guard = Arc::clone(&slot.server).try_lock_owned().ok()?;
                    Some((key.clone(), *slot.last_used.lock(), guard))
                })
                .min_by_key(|(_, last_used, _)| *last_used);
            let Some((key, _, mut guard)) = idle else {
                return Err(LspError::Busy(self.max));
            };
            slots.remove(&key);
            evicted = guard.take();
        }
        let slot = Slot::new();
        slots.insert(spec.clone(), Arc::clone(&slot));
        Ok((slot, evicted))
    }

    fn holds(&self, spec: &ServerSpec, slot: &Arc<Slot>) -> bool {
        self.slots
            .lock()
            .get(spec)
            .is_some_and(|held| Arc::ptr_eq(held, slot))
    }

    /// Drop a slot whose server did not start. Called with the slot's lock
    /// held, so a call waiting on it finds it gone and takes a new one.
    fn forget(&self, spec: &ServerSpec, slot: &Arc<Slot>) {
        let mut slots = self.slots.lock();
        if slots.get(spec).is_some_and(|held| Arc::ptr_eq(held, slot)) {
            slots.remove(spec);
        }
    }

    /// Servers running now.
    pub fn running(&self) -> usize {
        self.slots.lock().len()
    }

    /// Stop every server idle past the limit or already gone.
    pub fn reap(&self) {
        let mut slots = self.slots.lock();
        slots.retain(|_, slot| {
            let Ok(mut guard) = Arc::clone(&slot.server).try_lock_owned() else {
                return true;
            };
            let idle = slot.last_used.lock().elapsed() >= self.idle;
            match guard.as_ref() {
                Some(server) if server.is_alive() && !idle => true,
                _ => {
                    if let Some(server) = guard.take() {
                        info!(server = server.spec.kind.name(), root = %server.spec.root.display(), "language server stopped while idle");
                        tokio::spawn(server.shutdown());
                    }
                    false
                }
            }
        });
    }

    fn start_reaper(self: &Arc<Self>) {
        self.reaper.get_or_init(|| {
            let pool: Weak<Self> = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(REAP_EVERY);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    match pool.upgrade() {
                        Some(pool) => pool.reap(),
                        None => return,
                    }
                }
            });
        });
    }
}
