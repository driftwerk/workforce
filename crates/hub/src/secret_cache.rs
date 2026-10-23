//! In-memory implementation of [`workforce_common::secret_marker::SecretCache`].
//!
//! Storage shape: `DashMap<session_key, Mutex<SessionEntry>>`. Each
//! session holds a bounded `HashMap<MarkerId, Record>` plus a small LRU
//! list for eviction when the cap is exceeded.
//!
//! Lifetime model:
//!  * Per-record TTL with sliding expiry — every successful lookup
//!    extends the record's life by another full TTL window. Active
//!    conversations don't drop their own markers; abandoned ones age
//!    out.
//!  * `gc()` is the explicit sweep entry-point; the hub spawns a
//!    background task that calls it periodically. We don't sweep on
//!    every lookup because that would scale poorly with session count.
//!  * `purge_session` is called when a session ends or `/new` is used;
//!    expected to be cheap (drops the whole entry).
//!
//! Audit trail: every state transition writes to `sentinel.audit` with
//! the marker id, label, source, and outcome — but never the cleartext.
//! The `Secret<String>` wrapper means accidental `?value` formatting in
//! a log line yields `Secret(***)` rather than the credential itself.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use workforce_common::secret::Secret;
use workforce_common::secret_marker::{
    ConsumedSecret, Marker, MarkerId, SecretCache, SecretSource, MIN_KNOWN_REDACT_LEN,
};

/// Tunable knobs for the cache. Defaults are chosen for an interactive
/// chat session: a 60-minute idle window comfortably outlasts a single
/// setup conversation, and 16 markers per session is well above any
/// realistic legitimate use (a typical onboarding pastes 1–4 secrets).
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Idle TTL — refreshed on every successful `lookup`.
    pub ttl: Duration,
    /// Hard ceiling on live markers per session. When exceeded, the
    /// oldest record is evicted (with audit log) to make room.
    pub max_per_session: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(60 * 60),
            max_per_session: 16,
        }
    }
}

/// Time provider abstraction so tests can run without sleeping. The
/// production [`SystemClock`] reads `Instant::now()`; tests use a
/// hand-advanced clock so TTL behaviour is exercised in microseconds.
trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

struct Record {
    secret: Secret<String>,
    label: String,
    preview: String,
    source: SecretSource,
    expires_at: Instant,
}

/// FIFO order is enough for "evict the oldest" — true LRU would
/// reorder on read but adds no value at a 16-entry bound, and FIFO
/// gives clearer audit semantics ("the first secret you pasted was
/// dropped because you pasted too many").
#[derive(Default)]
struct SessionEntry {
    records: HashMap<MarkerId, Record>,
    /// Insertion order. Stale ids (record dropped via `forget`) are
    /// skipped at eviction time rather than eagerly cleaned.
    order: VecDeque<MarkerId>,
}

impl SessionEntry {
    fn evict_oldest(&mut self) -> Option<(MarkerId, Record)> {
        while let Some(id) = self.order.pop_front() {
            if let Some(rec) = self.records.remove(&id) {
                return Some((id, rec));
            }
        }
        None
    }
}

/// The key under which named secrets are stored and resolved.
///
/// Slack session keys are `slack:{workspace}:{channel}[:{thread|slash}]`;
/// the channel scope is the first three segments, so a value set via a
/// slash command (`…:slash`) is reachable from the channel root and any
/// thread within it. Keys from other surfaces have no such suffix
/// convention and scope to themselves.
fn channel_scope(session_key: &str) -> &str {
    const PREFIX: &str = "slack:";
    let Some(rest) = session_key.strip_prefix(PREFIX) else {
        return session_key;
    };
    match rest.match_indices(':').nth(1) {
        Some((idx, _)) => &session_key[..PREFIX.len() + idx],
        None => session_key,
    }
}

pub struct InMemorySecretCache {
    sessions: DashMap<String, Arc<Mutex<SessionEntry>>>,
    config: CacheConfig,
    clock: Arc<dyn Clock>,
}

impl InMemorySecretCache {
    pub fn new(config: CacheConfig) -> Self {
        Self::with_clock(config, Arc::new(SystemClock))
    }

    pub fn with_defaults() -> Self {
        Self::new(CacheConfig::default())
    }

    fn with_clock(config: CacheConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            sessions: DashMap::new(),
            config,
            clock,
        }
    }

    /// Sweep expired records across all sessions. Returns the count
    /// removed. Intended to be called from a background task at a
    /// modest cadence (e.g. every 5 minutes).
    pub fn gc(&self) -> usize {
        let now = self.clock.now();
        let mut removed = 0usize;
        // Collect session keys first; releasing the dashmap iterator
        // before per-session work avoids holding shard locks across
        // mutex acquisition or shard write-back.
        let keys: Vec<String> = self.sessions.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            // Take an Arc to the inner Mutex and release the dashmap
            // shard lock before locking it; otherwise the later
            // `sessions.remove(&key)` would deadlock on the same shard.
            let entry = match self.sessions.get(&key) {
                Some(e) => Arc::clone(&*e),
                None => continue,
            };
            let mut guard = entry.lock();
            let expired: Vec<MarkerId> = guard
                .records
                .iter()
                .filter(|(_, r)| now > r.expires_at)
                .map(|(id, _)| id.clone())
                .collect();
            for id in expired {
                if let Some(rec) = guard.records.remove(&id) {
                    info!(
                        target: "sentinel.audit",
                        session_key = %key,
                        marker_id = %id,
                        label = %rec.label,
                        "secret marker expired"
                    );
                    removed += 1;
                }
            }
            let now_empty = guard.records.is_empty();
            drop(guard);
            if now_empty {
                self.sessions.remove(&key);
            }
        }
        if removed > 0 {
            debug!(target: "sentinel.audit", removed, "secret cache gc swept");
        }
        removed
    }

    /// Spawn a background sweep task. Returns the JoinHandle so the
    /// caller can abort it on shutdown if needed; in practice the hub
    /// just drops the handle (the task is keyed off the cache's
    /// lifetime, which equals the process lifetime).
    pub fn spawn_gc_task(self: &Arc<Self>, period: Duration) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.tick().await; // skip the immediate firing
            loop {
                interval.tick().await;
                let Some(strong) = weak.upgrade() else {
                    break; // cache dropped, exit cleanly
                };
                strong.gc();
            }
        })
    }

    /// Insert `record` under `key`, enforcing the per-key cap. Re-using
    /// an existing id overwrites in place (no eviction, no duplicate
    /// order entry); a new id evicts the oldest when at capacity.
    fn store(&self, key: &str, id: MarkerId, record: Record) {
        let entry = self
            .sessions
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(SessionEntry::default())))
            .clone();
        let mut guard = entry.lock();
        let is_new = !guard.records.contains_key(&id);
        if is_new {
            while guard.records.len() >= self.config.max_per_session {
                let Some((evicted_id, evicted_rec)) = guard.evict_oldest() else {
                    break;
                };
                warn!(
                    target: "sentinel.audit",
                    session_key = %key,
                    marker_id = %evicted_id,
                    label = %evicted_rec.label,
                    cap = self.config.max_per_session,
                    "secret marker evicted (per-session cap exceeded)"
                );
            }
            guard.order.push_back(id.clone());
        }
        guard.records.insert(id, record);
    }

    /// Read a record under an exact key, sliding its TTL on a hit.
    fn lookup_in(&self, key: &str, id: &MarkerId) -> Option<(Secret<String>, ConsumedSecret)> {
        let entry = self.sessions.get(key)?;
        let mut guard = entry.lock();
        let record = guard.records.get_mut(id)?;
        record.expires_at = self.clock.now() + self.config.ttl;
        let consumed = ConsumedSecret {
            id: id.clone(),
            label: record.label.clone(),
            preview: record.preview.clone(),
            source: record.source.clone(),
        };
        debug!(
            target: "sentinel.audit",
            session_key = %key,
            marker_id = %id,
            label = %consumed.label,
            "secret marker resolved"
        );
        Some((record.secret.clone(), consumed))
    }

    /// Collect `(cleartext, marker-render)` pairs for every live record
    /// under `key`. `named_only` restricts to named markers (used for the
    /// channel-root scope, which legitimately holds only `/secret`
    /// values). Records below [`MIN_KNOWN_REDACT_LEN`] are skipped so
    /// exact-match redaction can't mangle ordinary text. Never logs or
    /// returns cleartext beyond the `subs` it builds for the caller's
    /// in-place replacement.
    fn collect_known_subs(
        &self,
        key: &str,
        named_only: bool,
        now: Instant,
        subs: &mut Vec<(String, String)>,
    ) {
        let Some(entry) = self.sessions.get(key) else {
            return;
        };
        let guard = entry.lock();
        for (id, rec) in guard.records.iter() {
            if rec.expires_at <= now || (named_only && !id.is_named()) {
                continue;
            }
            let value = rec.secret.expose_secret();
            if value.chars().count() < MIN_KNOWN_REDACT_LEN {
                continue;
            }
            let marker = Marker {
                id: id.clone(),
                label: rec.label.clone(),
                preview: rec.preview.clone(),
            };
            subs.push((value.to_string(), marker.render()));
        }
    }

    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.sessions.len()
    }

    #[cfg(test)]
    fn marker_count(&self, session_key: &str) -> usize {
        self.sessions
            .get(session_key)
            .map(|e| e.lock().records.len())
            .unwrap_or(0)
    }
}

impl SecretCache for InMemorySecretCache {
    fn capture(
        &self,
        session_key: &str,
        cleartext: Secret<String>,
        label: &str,
        source: SecretSource,
    ) -> MarkerId {
        let id = MarkerId::derived(session_key, cleartext.expose_secret());
        let preview = workforce_common::secret_marker::preview_of(cleartext.expose_secret());
        let record = Record {
            secret: cleartext,
            label: label.to_string(),
            preview: preview.clone(),
            source: source.clone(),
            expires_at: self.clock.now() + self.config.ttl,
        };
        self.store(session_key, id.clone(), record);

        info!(
            target: "sentinel.audit",
            session_key = %session_key,
            marker_id = %id,
            label = %label,
            preview = %preview,
            source = ?source,
            "secret captured"
        );

        id
    }

    fn capture_named(
        &self,
        session_key: &str,
        id: &MarkerId,
        cleartext: Secret<String>,
        source: SecretSource,
    ) {
        let scope = channel_scope(session_key);
        let record = Record {
            secret: cleartext,
            label: workforce_common::secret_marker::NAMED_LABEL.to_string(),
            preview: "***".to_string(),
            source: source.clone(),
            expires_at: self.clock.now() + self.config.ttl,
        };
        self.store(scope, id.clone(), record);

        info!(
            target: "sentinel.audit",
            scope = %scope,
            marker_id = %id,
            source = ?source,
            "named secret captured"
        );
    }

    fn lookup(&self, session_key: &str, id: &MarkerId) -> Option<(Secret<String>, ConsumedSecret)> {
        if let Some(hit) = self.lookup_in(session_key, id) {
            return Some(hit);
        }
        // Named secrets live at channel scope so a value set via
        // `/secret` (`…:slash`) resolves from threads and the channel
        // root. Auto ids stay strictly session-scoped.
        let scope = channel_scope(session_key);
        if scope != session_key && id.is_named() {
            self.lookup_in(scope, id)
        } else {
            None
        }
    }

    fn redact_known(&self, session_key: &str, text: &str) -> String {
        if text.is_empty() {
            return text.to_string();
        }
        let now = self.clock.now();
        let mut subs: Vec<(String, String)> = Vec::new();
        // The exact session key holds this session's auto + named markers;
        // the channel root (if distinct) legitimately holds only `/secret`
        // named values reachable from every thread in the channel.
        self.collect_known_subs(session_key, false, now, &mut subs);
        let scope = channel_scope(session_key);
        if scope != session_key {
            self.collect_known_subs(scope, true, now, &mut subs);
        }
        if subs.is_empty() {
            return text.to_string();
        }
        // Longest value first: a value that contains a shorter captured
        // value as a substring must be rewritten before its fragment, or
        // the inner replacement would corrupt the marker we just inserted.
        subs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let mut result = text.to_string();
        for (value, marker) in subs {
            if result.contains(&value) {
                result = result.replace(&value, &marker);
            }
        }
        result
    }

    fn list_named(&self, session_key: &str) -> Vec<String> {
        let Some(entry) = self.sessions.get(channel_scope(session_key)) else {
            return Vec::new();
        };
        let now = self.clock.now();
        let mut names: Vec<String> = entry
            .lock()
            .records
            .iter()
            .filter(|(id, rec)| id.is_named() && rec.expires_at > now)
            .map(|(id, _)| id.as_str().to_string())
            .collect();
        names.sort();
        names
    }

    fn purge_session(&self, session_key: &str) {
        if let Some((_, entry)) = self.sessions.remove(session_key) {
            let count = entry.lock().records.len();
            if count > 0 {
                info!(
                    target: "sentinel.audit",
                    session_key = %session_key,
                    marker_count = count,
                    "secret cache session purged"
                );
            }
        }
    }

    fn forget(&self, session_key: &str, id: &MarkerId) {
        if let Some(entry) = self.sessions.get(session_key) {
            let mut guard = entry.lock();
            if let Some(rec) = guard.records.remove(id) {
                info!(
                    target: "sentinel.audit",
                    session_key = %session_key,
                    marker_id = %id,
                    label = %rec.label,
                    "secret marker forgotten (explicit)"
                );
            }
        }
    }
}

/// The process-wide secret cache, installed for one test at a time.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use workforce_common::secret_marker::{clear_global_cache, set_global_cache, SecretCache};

    static INSTALLED: parking_lot::Mutex<()> = parking_lot::const_mutex(());

    /// Holds the global cache for the test that installed it, and clears it
    /// when dropped, a panicking test included.
    pub(crate) struct GlobalCache {
        _held: parking_lot::MutexGuard<'static, ()>,
    }

    impl GlobalCache {
        pub(crate) fn install(cache: Arc<dyn SecretCache>) -> Self {
            let _held = INSTALLED.lock();
            set_global_cache(cache);
            Self { _held }
        }
    }

    impl Drop for GlobalCache {
        fn drop(&mut self) {
            clear_global_cache();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-advanced clock so TTL behaviour can be exercised without
    /// real sleeps. All test cases use this; production code uses
    /// [`SystemClock`].
    struct TestClock(parking_lot::Mutex<Instant>);

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self(parking_lot::Mutex::new(Instant::now())))
        }
        fn advance(&self, d: Duration) {
            *self.0.lock() += d;
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            *self.0.lock()
        }
    }

    fn fresh_cache(ttl: Duration, cap: usize) -> (InMemorySecretCache, Arc<TestClock>) {
        let clock = TestClock::new();
        let cache = InMemorySecretCache::with_clock(
            CacheConfig {
                ttl,
                max_per_session: cap,
            },
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        (cache, clock)
    }

    fn slack_source() -> SecretSource {
        SecretSource::Slack {
            workspace_id: "T1".into(),
            channel: "C1".into(),
            message_ts: "1.0".into(),
            user: "U1".into(),
        }
    }

    fn ant(label: &str) -> Secret<String> {
        Secret::new(format!("sk-ant-very-secret-{label}-1234567890"))
    }

    #[test]
    fn capture_then_lookup_returns_cleartext() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());

        let (cleartext, meta) = cache.lookup("sess-1", &id).expect("present");
        assert_eq!(
            cleartext.expose_secret(),
            concat!("sk-ant", "-very-secret-value-1234567890")
        );
        assert_eq!(meta.label, "ANTHROPIC_KEY");
        assert_eq!(meta.id, id);
    }

    #[test]
    fn lookup_unknown_session_returns_none() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-A", ant("value"), "ANTHROPIC_KEY", slack_source());
        assert!(cache.lookup("sess-B", &id).is_none());
    }

    #[test]
    fn lookup_unknown_id_returns_none() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let _ = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        assert!(cache.lookup("sess-1", &MarkerId::fresh()).is_none());
    }

    #[test]
    fn cross_session_isolation() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id_a = cache.capture("sess-A", ant("aaaa"), "ANTHROPIC_KEY", slack_source());
        let id_b = cache.capture("sess-B", ant("bbbb"), "ANTHROPIC_KEY", slack_source());
        assert!(cache.lookup("sess-A", &id_b).is_none());
        assert!(cache.lookup("sess-B", &id_a).is_none());
        assert_eq!(
            cache.lookup("sess-A", &id_a).unwrap().0.expose_secret(),
            concat!("sk-ant", "-very-secret-aaaa-1234567890")
        );
    }

    #[test]
    fn purge_session_drops_all_markers() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        cache.purge_session("sess-1");
        assert!(cache.lookup("sess-1", &id).is_none());
        assert_eq!(cache.session_count(), 0);
    }

    #[test]
    fn forget_drops_single_marker_keeps_others() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id1 = cache.capture("sess-1", ant("one"), "ANTHROPIC_KEY", slack_source());
        let id2 = cache.capture("sess-1", ant("two"), "ANTHROPIC_KEY", slack_source());
        cache.forget("sess-1", &id1);
        assert!(cache.lookup("sess-1", &id1).is_none());
        assert!(cache.lookup("sess-1", &id2).is_some());
    }

    #[test]
    fn capturing_the_same_value_twice_in_a_session_keeps_one_record() {
        let (cache, clock) = fresh_cache(Duration::from_secs(60), 8);
        let first = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        clock.advance(Duration::from_secs(50));
        let second = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());

        assert_eq!(first, second);
        assert_eq!(cache.marker_count("sess-1"), 1);
        // Past the first capture's expiry but inside the second's: the
        // re-capture refreshed the record rather than adding one.
        clock.advance(Duration::from_secs(20));
        assert_eq!(cache.gc(), 0);
        assert_eq!(
            cache.lookup("sess-1", &first).unwrap().0.expose_secret(),
            concat!("sk-ant", "-very-secret-value-1234567890")
        );
    }

    #[test]
    fn the_same_value_in_two_sessions_gets_unrelated_ids() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id_a = cache.capture("sess-A", ant("value"), "ANTHROPIC_KEY", slack_source());
        let id_b = cache.capture("sess-B", ant("value"), "ANTHROPIC_KEY", slack_source());
        assert_ne!(id_a, id_b);
        assert!(cache.lookup("sess-A", &id_b).is_none());
        assert!(cache.lookup("sess-B", &id_a).is_none());
    }

    #[test]
    fn ttl_sliding_lookup_extends_life() {
        let (cache, clock) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        // Half-way: still alive, lookup extends.
        clock.advance(Duration::from_secs(40));
        assert!(cache.lookup("sess-1", &id).is_some());
        // Past the original 60s window, but the touch reset it.
        clock.advance(Duration::from_secs(40));
        assert!(cache.lookup("sess-1", &id).is_some());
    }

    #[test]
    fn gc_evicts_expired_records() {
        let (cache, clock) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        clock.advance(Duration::from_secs(61));
        let removed = cache.gc();
        assert_eq!(removed, 1);
        assert!(cache.lookup("sess-1", &id).is_none());
        // Empty session entry should be dropped from the dashmap.
        assert_eq!(cache.session_count(), 0);
    }

    #[test]
    fn gc_keeps_live_records() {
        let (cache, clock) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        clock.advance(Duration::from_secs(30));
        assert_eq!(cache.gc(), 0);
        assert!(cache.lookup("sess-1", &id).is_some());
    }

    #[test]
    fn capture_beyond_cap_evicts_oldest_fifo() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 2);
        let id1 = cache.capture("sess-1", ant("one"), "ANTHROPIC_KEY", slack_source());
        let id2 = cache.capture("sess-1", ant("two"), "ANTHROPIC_KEY", slack_source());
        let id3 = cache.capture("sess-1", ant("three"), "ANTHROPIC_KEY", slack_source());
        assert!(cache.lookup("sess-1", &id1).is_none());
        assert!(cache.lookup("sess-1", &id2).is_some());
        assert!(cache.lookup("sess-1", &id3).is_some());
        assert_eq!(cache.marker_count("sess-1"), 2);
    }

    #[test]
    fn forget_clears_order_entry_for_eviction() {
        // After `forget(id1)`, capturing past the cap must not error or
        // evict id2 first; the order deque correctly skips dead ids.
        let (cache, _) = fresh_cache(Duration::from_secs(60), 2);
        let id1 = cache.capture("sess-1", ant("one"), "ANTHROPIC_KEY", slack_source());
        let id2 = cache.capture("sess-1", ant("two"), "ANTHROPIC_KEY", slack_source());
        cache.forget("sess-1", &id1);
        let id3 = cache.capture("sess-1", ant("three"), "ANTHROPIC_KEY", slack_source());
        assert!(cache.lookup("sess-1", &id2).is_some());
        assert!(cache.lookup("sess-1", &id3).is_some());
        assert_eq!(cache.marker_count("sess-1"), 2);
    }

    fn named(name: &str) -> MarkerId {
        MarkerId::named(name).expect("valid name")
    }

    fn slash_source() -> SecretSource {
        SecretSource::SlashCommand { user: "U1".into() }
    }

    #[test]
    fn channel_scope_strips_slack_suffix() {
        assert_eq!(channel_scope("slack:T1:C1:slash"), "slack:T1:C1");
        assert_eq!(channel_scope("slack:T1:C1:1700000000.001"), "slack:T1:C1");
        assert_eq!(channel_scope("slack:T1:C1"), "slack:T1:C1");
        assert_eq!(channel_scope("web-session-abc"), "web-session-abc");
    }

    #[test]
    fn named_secret_resolves_from_channel_root_and_threads() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = named("datadog_api_key");
        cache.capture_named(
            "slack:T1:C1:slash",
            &id,
            Secret::new("ddapp_secret_value_0001".into()),
            slash_source(),
        );
        for consumer in [
            "slack:T1:C1",
            "slack:T1:C1:slash",
            "slack:T1:C1:1700000000.5",
        ] {
            assert_eq!(
                cache
                    .lookup(consumer, &id)
                    .expect(consumer)
                    .0
                    .expose_secret(),
                "ddapp_secret_value_0001",
                "named secret should resolve from {consumer}"
            );
        }
    }

    #[test]
    fn named_secret_is_isolated_per_channel() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = named("api_key");
        cache.capture_named(
            "slack:T1:C1:slash",
            &id,
            Secret::new("value-for-c1-channel".into()),
            slash_source(),
        );
        assert!(cache.lookup("slack:T1:C2:1700000000.5", &id).is_none());
        assert!(cache.lookup("slack:T2:C1", &id).is_none());
    }

    #[test]
    fn re_set_named_secret_overwrites_without_duplicate_order() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = named("token");
        cache.capture_named(
            "slack:T1:C1:slash",
            &id,
            Secret::new("first-value-aaaaaaaaa".into()),
            slash_source(),
        );
        cache.capture_named(
            "slack:T1:C1:slash",
            &id,
            Secret::new("second-value-bbbbbbbb".into()),
            slash_source(),
        );
        assert_eq!(
            cache.lookup("slack:T1:C1", &id).unwrap().0.expose_secret(),
            "second-value-bbbbbbbb"
        );
        assert_eq!(cache.marker_count("slack:T1:C1"), 1);
    }

    #[test]
    fn list_named_returns_sorted_names_excluding_auto_and_expired() {
        let (cache, clock) = fresh_cache(Duration::from_secs(60), 8);
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("zebra_key"),
            Secret::new("v1-aaaaaaaaaaaaaaaa".into()),
            slash_source(),
        );
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("alpha_key"),
            Secret::new("v2-bbbbbbbbbbbbbbbb".into()),
            slash_source(),
        );
        // An auto secret captured at the channel root must not appear.
        cache.capture("slack:T1:C1", ant("auto"), "ANTHROPIC_KEY", slack_source());

        // Visible from a thread (channel scope), sorted, names only.
        assert_eq!(
            cache.list_named("slack:T1:C1:1700000000.5"),
            vec!["alpha_key".to_string(), "zebra_key".to_string()]
        );

        // Expired named secrets drop out.
        clock.advance(Duration::from_secs(61));
        assert!(cache.list_named("slack:T1:C1").is_empty());
    }

    #[test]
    fn list_named_is_channel_isolated_and_empty_by_default() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("k"),
            Secret::new("value-aaaaaaaaaaaaa".into()),
            slash_source(),
        );
        assert!(cache.list_named("slack:T1:C2").is_empty());
        assert!(cache.list_named("slack:T2:C1").is_empty());
        assert!(cache.list_named("web-session-xyz").is_empty());
    }

    #[test]
    fn auto_id_does_not_use_channel_fallback() {
        // An auto id captured at the channel root must NOT leak into a
        // thread lookup — only named ids widen to channel scope.
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("slack:T1:C1", ant("v"), "ANTHROPIC_KEY", slack_source());
        assert!(cache.lookup("slack:T1:C1:1700000000.5", &id).is_none());
    }

    // ── redact_known: by-value scrub of re-entered cleartext ──────────
    // The incident: a `/secret monolith_db_pass <password>` typed as a Slack
    // message persists in the thread; on a later turn the raw line is
    // re-hydrated and the model echoes the password. The value is an
    // arbitrary string matching no credential pattern, so only exact-match
    // against the cache can catch it.

    #[test]
    fn redact_known_masks_arbitrary_named_value_from_thread() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        // Deliberately unpatternable — would defeat pattern-based redaction.
        let password = "p@ssw0rd-Not-A-Known-Pattern-9z";
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("monolith_db_pass"),
            Secret::new(password.into()),
            slash_source(),
        );
        let raw = format!("U1: /secret monolith_db_pass {password}\nU1: monolith_prod please");
        // Re-read from a *thread* in the same channel (named secrets widen
        // to channel scope, exactly like lookup).
        let scrubbed = cache.redact_known("slack:T1:C1:1700000000.5", &raw);
        assert!(
            !scrubbed.contains(password),
            "cleartext password must not survive: {scrubbed}"
        );
        assert!(
            scrubbed.contains("[[secret:monolith_db_pass]]"),
            "value should be replaced by its marker: {scrubbed}"
        );
    }

    #[test]
    fn redact_known_masks_auto_marker_value_in_session() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let id = cache.capture("sess-1", ant("xyz"), "ANTHROPIC_KEY", slack_source());
        let value = concat!("sk-ant", "-very-secret-xyz-1234567890");
        let scrubbed = cache.redact_known("sess-1", &format!("the key is {value} ok"));
        assert!(!scrubbed.contains(value), "auto value leaked: {scrubbed}");
        assert!(
            scrubbed.contains(&format!("[[secret:{id}")),
            "auto value should become its marker: {scrubbed}"
        );
    }

    #[test]
    fn redact_known_skips_short_values_to_avoid_corrupting_prose() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        // 4 chars — below MIN_KNOWN_REDACT_LEN; redacting it would mangle
        // ordinary text that happens to contain "test".
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("pin"),
            Secret::new("test".into()),
            slash_source(),
        );
        let raw = "let me test the connection";
        assert_eq!(
            cache.redact_known("slack:T1:C1:1700000000.5", raw),
            raw,
            "short secret must not trigger redaction of ordinary words"
        );
    }

    #[test]
    fn redact_known_auto_value_is_session_scoped() {
        // Auto markers don't widen to channel scope (mirrors `lookup`); a
        // re-read in a different thread won't exact-match them — that case
        // is already covered by pattern re-detection on the way in.
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        cache.capture("slack:T1:C1", ant("v"), "ANTHROPIC_KEY", slack_source());
        let value = concat!("sk-ant", "-very-secret-v-1234567890");
        let other_thread = format!("echo {value}");
        assert_eq!(
            cache.redact_known("slack:T1:C1:1700000000.5", &other_thread),
            other_thread
        );
    }

    #[test]
    fn redact_known_no_live_secrets_is_passthrough() {
        let (cache, _) = fresh_cache(Duration::from_secs(60), 8);
        let raw = "nothing secret here, just words";
        assert_eq!(cache.redact_known("sess-1", raw), raw);
    }

    #[test]
    fn redact_known_skips_expired_named_value() {
        let (cache, clock) = fresh_cache(Duration::from_secs(60), 8);
        let password = "expired-but-unpatternable-value-1";
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("old_key"),
            Secret::new(password.into()),
            slash_source(),
        );
        clock.advance(Duration::from_secs(61));
        // Expired record is not a live secret; nothing to scrub. (The raw
        // text is unchanged, but the value is also no longer resolvable at
        // a sink, so there is no usable credential to protect.)
        let raw = format!("leftover {password}");
        assert_eq!(cache.redact_known("slack:T1:C1", &raw), raw);
    }

    #[test]
    fn install_as_global_cache() {
        use workforce_common::secret_marker::global_cache;
        let cache: Arc<dyn SecretCache> = Arc::new(InMemorySecretCache::with_defaults());
        let _installed = super::testing::GlobalCache::install(Arc::clone(&cache));
        let g = global_cache().expect("set");
        let id = g.capture("sess-1", ant("value"), "ANTHROPIC_KEY", slack_source());
        let (ct, _meta) = g.lookup("sess-1", &id).expect("present");
        assert_eq!(
            ct.expose_secret(),
            concat!("sk-ant", "-very-secret-value-1234567890")
        );
    }

    /// End-to-end: `tokenize_secrets` (the redaction point every tool
    /// result flows through) must scrub a previously-captured arbitrary
    /// `/secret` value re-surfaced as raw text — e.g. a `slack.get_message_replies`
    /// result containing the persisted `/secret …` line.
    #[test]
    fn tokenize_secrets_scrubs_known_named_value_via_global_cache() {
        let cache: Arc<dyn SecretCache> = Arc::new(InMemorySecretCache::with_defaults());
        let _installed = super::testing::GlobalCache::install(Arc::clone(&cache));

        let password = "p@ssw0rd-Not-A-Known-Pattern-9z";
        cache.capture_named(
            "slack:T1:C1:slash",
            &named("monolith_db_pass"),
            Secret::new(password.into()),
            slash_source(),
        );

        // Simulate a tool result re-reading the thread, tokenized at the
        // thread's session key.
        let tool_output = format!("U1: /secret monolith_db_pass {password}");
        let (scrubbed, _captured) = workforce_common::redact::tokenize_secrets(
            &tool_output,
            "slack:T1:C1:1700000000.5",
            slack_source(),
        );

        assert!(
            !scrubbed.contains(password),
            "tokenize_secrets leaked cleartext: {scrubbed}"
        );
        assert!(
            scrubbed.contains("[[secret:monolith_db_pass]]"),
            "tokenize_secrets should mask via the marker: {scrubbed}"
        );
    }
}
