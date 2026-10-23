use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eyre::Result;
use notify::{RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Filesystem event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsEventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// A filesystem change event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEvent {
    pub kind: FsEventKind,
    pub path: String,
    pub timestamp: String,
}

/// Filesystem watcher configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsWatcherConfig {
    /// Paths to watch.
    pub paths: Vec<String>,
    /// File-name patterns to include (glob). Empty = all.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// How long a burst of changes must stay quiet before it is reported.
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
    /// Whether to watch below the paths too.
    #[serde(default)]
    pub recursive: bool,
}

fn default_debounce() -> u64 {
    500
}

/// The change `event` reports, if it is one. Its paths are already those the
/// subscriber watches and matches, so a rename reports whichever side of it
/// that is, and a file replaced by renaming another over it counts as changed.
fn relevant(event: notify::Event) -> Option<FsEvent> {
    use notify::event::{EventKind, ModifyKind};
    let kind = match event.kind {
        EventKind::Create(_) => FsEventKind::Created,
        EventKind::Modify(ModifyKind::Name(_)) => FsEventKind::Renamed,
        EventKind::Modify(_) => FsEventKind::Modified,
        EventKind::Remove(_) => FsEventKind::Deleted,
        _ => return None,
    };
    let path = event.paths.first()?;
    Some(FsEvent {
        kind,
        path: path.to_string_lossy().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}

fn matches(path: &Path, patterns: &[glob::Pattern]) -> bool {
    patterns.is_empty()
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| patterns.iter().any(|p| p.matches(name)))
}

/// Where a [`Watch`] gets its OS watcher.
#[derive(Clone, Copy)]
enum Strategy {
    /// One process-wide watcher: inotify instances are capped per user.
    Shared,
    /// A watcher per subscription.
    PerSubscription,
}

impl Strategy {
    /// FSEvents restarts its stream on every watch change, dropping pending
    /// events, so on macOS a shared watcher would blind every other watch.
    const OF_BACKEND: Self = if cfg!(target_os = "macos") {
        Self::PerSubscription
    } else {
        Self::Shared
    };

    /// The hub a new watch subscribes to; `None`, having said why, when no
    /// OS watcher could start.
    fn hub(self) -> Option<Arc<WatchHub>> {
        match self {
            Self::Shared => WatchHub::shared(),
            Self::PerSubscription => WatchHub::start()
                .inspect_err(|e| warn!(error = %e, "Filesystem watcher failed to start"))
                .ok()
                .map(Arc::new),
        }
    }
}

/// An OS watcher multiplexed across its subscribers.
struct WatchHub {
    watched: Mutex<Watched>,
    routes: Arc<Routes>,
    next_id: AtomicU64,
}

/// The paths the OS watcher is watching, with who holds each.
struct Watched {
    watcher: Box<dyn notify::Watcher + Send>,
    paths: HashMap<PathBuf, Holders>,
}

/// Every subscriber's [`Route`], by subscription.
#[derive(Default)]
struct Routes(Mutex<HashMap<u64, Route>>);

impl Routes {
    /// Hand what the OS watcher reports to every subscriber it concerns.
    fn dispatch(&self, event: notify::Result<notify::Event>) {
        match event {
            Ok(event) => self.0.lock().values().for_each(|route| route.offer(&event)),
            Err(e) => warn!("Filesystem watch error: {}", e),
        }
    }
}

#[derive(Default)]
struct Holders {
    recursive: usize,
    flat: usize,
}

/// Where the changes one subscriber watches for go.
struct Route {
    roots: Vec<PathBuf>,
    recursive: bool,
    patterns: Vec<glob::Pattern>,
    events: mpsc::UnboundedSender<notify::Event>,
}

impl Holders {
    fn count(&mut self, recursive: bool) -> &mut usize {
        if recursive {
            &mut self.recursive
        } else {
            &mut self.flat
        }
    }

    fn mode(&self) -> Option<RecursiveMode> {
        match (self.recursive, self.flat) {
            (0, 0) => None,
            (0, _) => Some(RecursiveMode::NonRecursive),
            _ => Some(RecursiveMode::Recursive),
        }
    }
}

impl Route {
    fn covers(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| {
            if self.recursive {
                path.starts_with(root)
            } else {
                path == root || path.parent() == Some(root)
            }
        })
    }

    /// Pass on the part of `event` this subscriber watches for, if any.
    fn offer(&self, event: &notify::Event) {
        let paths: Vec<PathBuf> = event
            .paths
            .iter()
            .filter(|path| self.covers(path) && matches(path, &self.patterns))
            .cloned()
            .collect();
        if !paths.is_empty() {
            let _ = self.events.send(notify::Event {
                kind: event.kind,
                paths,
                attrs: event.attrs.clone(),
            });
        }
    }
}

impl Watched {
    fn hold(&mut self, path: &Path, recursive: bool) -> Result<()> {
        let holders = self.paths.entry(path.to_path_buf()).or_default();
        let before = holders.mode();
        *holders.count(recursive) += 1;
        let after = holders.mode();
        if before == after {
            return Ok(());
        }
        let watched = after.map_or(Ok(()), |mode| self.watcher.watch(path, mode));
        watched.map_err(|e| {
            self.release(path, recursive);
            eyre::eyre!("Failed to watch {}: {}", path.display(), e)
        })
    }

    /// Let go of one hold on `path`: the OS stops watching it with its last,
    /// and narrows to its directory once nobody watches below it. Paths
    /// nested with it are watched again, as inotify shares their watches.
    fn release(&mut self, path: &Path, recursive: bool) {
        let Some(holders) = self.paths.get_mut(path) else {
            return;
        };
        let before = holders.mode();
        let held = holders.count(recursive);
        *held = held.saturating_sub(1);
        let after = holders.mode();
        if after.is_none() {
            self.paths.remove(path);
        }
        if before == after {
            return;
        }
        if let Err(e) = self.watcher.unwatch(path) {
            debug!(path = %path.display(), error = %e, "Filesystem watch already gone");
        }
        let nested = self.paths.iter().filter_map(|(other, holders)| {
            let mode = holders.mode()?;
            (other == path
                || other.starts_with(path)
                || (mode == RecursiveMode::Recursive && path.starts_with(other)))
            .then_some((other, mode))
        });
        for (other, mode) in nested {
            if let Err(e) = self.watcher.watch(other, mode) {
                warn!(path = %other.display(), error = %e, "Failed to keep watching filesystem path");
            }
        }
    }
}

impl WatchHub {
    /// The process's watch hub, started on first use; `None`, having said
    /// why once, when the OS watcher could not start.
    fn shared() -> Option<Arc<Self>> {
        static HUB: OnceLock<Option<Arc<WatchHub>>> = OnceLock::new();
        HUB.get_or_init(|| {
            Self::start()
                .inspect_err(|e| warn!(error = %e, "Filesystem watching unavailable; files will not hot-reload"))
                .ok()
                .map(Arc::new)
        })
        .clone()
    }

    fn start() -> Result<Self> {
        Self::over(|routes| {
            notify::RecommendedWatcher::new(
                move |event| routes.dispatch(event),
                notify::Config::default(),
            )
        })
    }

    /// A hub on the OS watcher `watcher` builds to report to its routes.
    fn over<W: notify::Watcher + Send + 'static>(
        watcher: impl FnOnce(Arc<Routes>) -> notify::Result<W>,
    ) -> Result<Self> {
        let routes = Arc::new(Routes::default());
        let watcher = watcher(Arc::clone(&routes))
            .map_err(|e| eyre::eyre!("Failed to create watcher: {}", e))?;
        Ok(Self {
            watched: Mutex::new(Watched {
                watcher: Box::new(watcher),
                paths: HashMap::new(),
            }),
            routes,
            next_id: AtomicU64::new(0),
        })
    }

    /// Watch `roots`, which must be canonical, as the OS reports real paths:
    /// every change from here on to a file under them `patterns` match goes
    /// to the receiver, until the [`Subscription`] is dropped.
    fn subscribe(
        self: &Arc<Self>,
        roots: Vec<PathBuf>,
        recursive: bool,
        patterns: Vec<glob::Pattern>,
    ) -> Result<(Subscription, mpsc::UnboundedReceiver<notify::Event>)> {
        let (events, received) = mpsc::unbounded_channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.routes.0.lock().insert(
            id,
            Route {
                roots: roots.clone(),
                recursive,
                patterns,
                events,
            },
        );
        let mut subscription = Subscription {
            hub: Arc::clone(self),
            id,
            roots: Vec::with_capacity(roots.len()),
            recursive,
        };
        for root in roots {
            self.watched.lock().hold(&root, recursive)?;
            subscription.roots.push(root);
        }
        Ok((subscription, received))
    }
}

/// One subscriber's share of its [`WatchHub`], ended by dropping it.
struct Subscription {
    hub: Arc<WatchHub>,
    id: u64,
    roots: Vec<PathBuf>,
    recursive: bool,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.routes.0.lock().remove(&self.id);
        let mut watched = self.hub.watched.lock();
        self.roots
            .iter()
            .for_each(|root| watched.release(root, self.recursive));
    }
}

/// A watch on its paths, live from [`Watch::on`]: its share of an OS
/// watcher and what it reports.
struct Watch {
    _subscription: Subscription,
    events: mpsc::UnboundedReceiver<notify::Event>,
    debounce: Duration,
}

impl Watch {
    /// Start watching `config.paths` on `hub`: every change from here on is
    /// seen.
    fn on(hub: Arc<WatchHub>, config: FsWatcherConfig) -> Result<Self> {
        let patterns = config
            .patterns
            .iter()
            .map(|p| glob::Pattern::new(p))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| eyre::eyre!("Invalid watch pattern: {}", e))?;
        let roots = config
            .paths
            .iter()
            .filter_map(|path| {
                std::fs::canonicalize(path)
                    .inspect(|_| info!(path = path, "Watching filesystem path"))
                    .inspect_err(
                        |e| warn!(path = path, error = %e, "Watch path does not exist, skipping"),
                    )
                    .ok()
            })
            .collect();
        let (subscription, events) = hub.subscribe(roots, config.recursive, patterns)?;
        Ok(Self {
            _subscription: subscription,
            events,
            debounce: Duration::from_millis(config.debounce_ms),
        })
    }

    /// Send one event for each burst of matching changes, once it has been
    /// quiet for the debounce: after the burst's last change, so a reader of
    /// the file sees what the burst left behind. Ends when `tx` closes, and
    /// its subscription with it.
    async fn run(mut self, tx: mpsc::Sender<FsEvent>) {
        loop {
            let first = tokio::select! {
                _ = tx.closed() => return,
                event = self.events.recv() => event,
            };
            let Some(first) = first else {
                return;
            };
            let Some(mut latest) = relevant(first) else {
                continue;
            };
            let quiet = tokio::time::sleep(self.debounce);
            tokio::pin!(quiet);
            loop {
                tokio::select! {
                    _ = &mut quiet => break,
                    event = self.events.recv() => match event {
                        Some(event) => {
                            if let Some(event) = relevant(event) {
                                latest = event;
                                quiet.as_mut().reset(tokio::time::Instant::now() + self.debounce);
                            }
                        }
                        None => return,
                    },
                }
            }
            debug!(path = %latest.path, kind = ?latest.kind, "Filesystem event");
            if tx.send(latest).await.is_err() {
                return;
            }
        }
    }
}

/// Keeps a watch started by [`spawn_on_change`] running: dropping it ends the
/// watch at once, without waiting for another change.
#[must_use = "the watch ends when this is dropped"]
pub struct Watching {
    _stop: tokio_util::sync::DropGuard,
}

/// Run `on_change` after each burst of changes `config` matches, from the
/// moment this returns until the [`Watching`] it returns is dropped, or
/// `on_change` returns `None` because whatever it acts on is gone. `None`
/// when the watch could not start.
pub fn spawn_on_change<F, Fut>(
    name: &'static str,
    config: FsWatcherConfig,
    on_change: F,
) -> Option<Watching>
where
    F: Fn() -> Option<Fut> + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let hub = Strategy::OF_BACKEND.hub()?;
    let watch = Watch::on(hub, config)
        .inspect_err(|e| warn!(watcher = name, error = %e, "FS watcher failed"))
        .ok()?;
    let (tx, mut rx) = mpsc::channel(1);
    tokio::spawn(watch.run(tx));
    let stop = tokio_util::sync::CancellationToken::new();
    let stopped = stop.clone();
    tokio::spawn(async move {
        while let Some(change) = tokio::select! {
            _ = stopped.cancelled() => None,
            changed = rx.recv() => changed.and_then(|_| on_change()),
        } {
            change.await;
        }
    });
    Some(Watching {
        _stop: stop.drop_guard(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, EventKind, ModifyKind, RenameMode};

    const WAIT: Duration = Duration::from_secs(5);
    const DEBOUNCE: Duration = Duration::from_millis(100);

    fn config_watcher(dir: &Path) -> FsWatcherConfig {
        FsWatcherConfig {
            paths: vec![dir.to_string_lossy().to_string()],
            patterns: vec!["config.toml".into()],
            debounce_ms: DEBOUNCE.as_millis() as u64,
            recursive: false,
        }
    }

    /// Watch `dir`'s `config.toml` the way this backend's hub does.
    fn start(dir: &Path) -> Watch {
        Watch::on(Strategy::OF_BACKEND.hub().unwrap(), config_watcher(dir)).unwrap()
    }

    /// What a hub asked of its OS watcher.
    #[derive(Debug, Clone, PartialEq)]
    enum Asked {
        Watch(PathBuf, RecursiveMode),
        Unwatch(PathBuf),
    }

    /// An OS watcher that reports nothing and records what it is asked.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Asked>>>);

    impl Recorder {
        fn asked(&self) -> Vec<Asked> {
            self.0.lock().clone()
        }
    }

    impl notify::Watcher for Recorder {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }

        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            self.0.lock().push(Asked::Watch(path.to_path_buf(), mode));
            Ok(())
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.0.lock().push(Asked::Unwatch(path.to_path_buf()));
            Ok(())
        }

        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    /// A hub that sees exactly the events a test dispatches to it, with the
    /// record of what it asks of its OS watcher. The OS's own delivery is not
    /// this module's to test, and FSEvents gives no bound on it: with
    /// `fseventsd` busy, a change has taken 20 s to arrive.
    fn injected() -> (Arc<WatchHub>, Recorder) {
        let os = Recorder::default();
        let hub = WatchHub::over(|_| Ok(os.clone())).unwrap();
        (Arc::new(hub), os)
    }

    /// Report `event` as the OS would, and let every watch take it in before
    /// the clock moves.
    async fn dispatch(hub: &WatchHub, event: notify::Event) {
        hub.routes.dispatch(Ok(event));
        tokio::task::yield_now().await;
    }

    /// `name` in `dir` as the OS reports it, by its real path.
    fn real(dir: &Path, name: &str) -> PathBuf {
        std::fs::canonicalize(dir).unwrap().join(name)
    }

    fn event(kind: EventKind, paths: &[&PathBuf]) -> notify::Event {
        paths.iter().fold(notify::Event::new(kind), |event, path| {
            event.add_path(path.to_path_buf())
        })
    }

    fn written(dir: &Path, name: &str) -> notify::Event {
        event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            &[&real(dir, name)],
        )
    }

    /// `config.toml` in `dir` replaced by writing a sibling and renaming it
    /// over the original, as each backend reports it: inotify pairs the two
    /// sides of the rename, FSEvents reports each path on its own.
    fn rename_replaces(dir: &Path) -> [Vec<notify::Event>; 2] {
        let (staged, config) = (&real(dir, "config.toml.new"), &real(dir, "config.toml"));
        let created = || event(EventKind::Create(CreateKind::File), &[staged]);
        let renamed =
            |mode, paths: &[&PathBuf]| event(EventKind::Modify(ModifyKind::Name(mode)), paths);
        [
            vec![
                created(),
                written(dir, "config.toml.new"),
                renamed(RenameMode::From, &[staged]),
                renamed(RenameMode::To, &[config]),
                renamed(RenameMode::Both, &[staged, config]),
            ],
            vec![
                created(),
                renamed(RenameMode::Any, &[staged]),
                renamed(RenameMode::Any, &[config]),
            ],
        ]
    }

    /// Watch `dir`'s `config.toml` on `hub`: what the watch reports, and its
    /// task.
    fn watching(
        hub: &Arc<WatchHub>,
        dir: &Path,
    ) -> (mpsc::Receiver<FsEvent>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(4);
        let watch = Watch::on(Arc::clone(hub), config_watcher(dir)).unwrap();
        (rx, tokio::spawn(watch.run(tx)))
    }

    async fn reported(rx: &mut mpsc::Receiver<FsEvent>) -> bool {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .is_ok_and(|event| event.is_some())
    }

    fn watch_of(dir: &Path) -> Asked {
        Asked::Watch(
            std::fs::canonicalize(dir).unwrap(),
            RecursiveMode::NonRecursive,
        )
    }

    #[test]
    fn the_shared_strategy_hands_every_watch_the_same_hub() {
        let hub = Strategy::Shared.hub().expect("the OS watcher starts");
        assert!(Arc::ptr_eq(&hub, &Strategy::Shared.hub().unwrap()));
    }

    #[tokio::test(start_paused = true)]
    async fn many_subscriptions_share_one_os_watcher() {
        let (hub, os) = injected();
        let dirs: Vec<_> = (0..8).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut watches: Vec<_> = dirs.iter().map(|dir| watching(&hub, dir.path())).collect();

        for (dir, (rx, _)) in dirs.iter().zip(&mut watches) {
            dispatch(&hub, written(dir.path(), "config.toml")).await;
            assert!(
                reported(rx).await,
                "{} saw its own change",
                dir.path().display()
            );
        }
        assert!(
            watches.iter_mut().all(|(rx, _)| rx.try_recv().is_err()),
            "none saw another's change"
        );
        assert_eq!(
            os.asked(),
            dirs.iter()
                .map(|dir| watch_of(dir.path()))
                .collect::<Vec<_>>(),
            "the one OS watcher watches them all"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_subscribers_to_one_directory_both_see_a_change_and_one_leaving_keeps_the_other() {
        let (hub, os) = injected();
        let dir = tempfile::tempdir().unwrap();
        let change = || dispatch(&hub, written(dir.path(), "config.toml"));
        let (mut leaving, left) = watching(&hub, dir.path());
        let (mut staying, stays) = watching(&hub, dir.path());

        change().await;
        assert!(reported(&mut leaving).await, "the first subscriber saw it");
        assert!(reported(&mut staying).await, "the second subscriber saw it");

        drop(leaving);
        tokio::time::timeout(WAIT, left).await.unwrap().unwrap();
        change().await;
        assert!(reported(&mut staying).await, "the one left still watches");
        assert_eq!(
            os.asked(),
            [watch_of(dir.path())],
            "the OS watches the directory once, and still does"
        );

        drop(staying);
        tokio::time::timeout(WAIT, stays).await.unwrap().unwrap();
        assert_eq!(
            os.asked().last(),
            Some(&Asked::Unwatch(std::fs::canonicalize(dir.path()).unwrap())),
            "nobody watches it any more"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rename_replace_is_reported_after_the_burst_and_the_watch_outlives_setup() {
        let (hub, _os) = injected();
        let dir = tempfile::tempdir().unwrap();
        let (mut rx, _watch) = watching(&hub, dir.path());

        for burst in rename_replaces(dir.path()) {
            let mut events = burst.into_iter().peekable();
            while let Some(event) = events.next() {
                dispatch(&hub, event).await;
                if events.peek().is_some() {
                    tokio::time::advance(DEBOUNCE / 2).await;
                    assert!(rx.try_recv().is_err(), "nothing is reported mid-burst");
                }
            }
            let settled = tokio::time::Instant::now() + DEBOUNCE;
            let event = tokio::time::timeout(WAIT, rx.recv())
                .await
                .expect("the replace is reported")
                .expect("the watch is still running");
            assert!(event.path.ends_with("config.toml"), "{event:?}");
            assert_eq!(
                tokio::time::Instant::now(),
                settled,
                "reported once the burst settled"
            );
        }

        dispatch(&hub, written(dir.path(), "sessions.jsonl")).await;
        assert!(
            tokio::time::timeout(DEBOUNCE * 10, rx.recv())
                .await
                .is_err(),
            "another file in the directory is not config.toml"
        );
    }

    #[tokio::test]
    async fn dropping_the_watching_ends_the_watch_without_another_change() {
        let dir = tempfile::tempdir().unwrap();
        let (held, released) = tokio::sync::oneshot::channel::<()>();
        let watching = spawn_on_change("test", config_watcher(dir.path()), move || {
            let _held = &held;
            None::<std::future::Ready<()>>
        })
        .expect("the watch starts");
        drop(watching);
        assert!(
            tokio::time::timeout(WAIT, released).await.is_ok(),
            "the watch let go of what it acts on with no change to the file"
        );
    }

    #[tokio::test]
    async fn the_watch_ends_when_its_reader_goes() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel(1);
        let watching = tokio::spawn(start(dir.path()).run(tx));
        drop(rx);
        tokio::time::timeout(WAIT, watching)
            .await
            .expect("the watch stops once nobody reads it")
            .unwrap();
    }

    #[test]
    fn patterns_match_file_names() {
        let patterns = [glob::Pattern::new("*.toml").unwrap()];
        assert!(matches(Path::new("/ws/wf/a.toml"), &patterns));
        assert!(!matches(Path::new("/ws/wf/a.toml.new"), &patterns));
        assert!(matches(Path::new("/ws/anything"), &[]));
    }
}
