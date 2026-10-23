use async_nats::jetstream::kv::{
    Config as KvConfig, CreateError, CreateErrorKind, EntryError, EntryErrorKind, Operation, Store,
    UpdateError, UpdateErrorKind,
};
use async_nats::jetstream::Context;
use async_trait::async_trait;
use bytes::Bytes;
use eyre::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const BUCKET_NAME: &str = "WORKFORCE_LEADER";
const LEADER_KEY: &str = "hub_leader";

/// TTL for the leader lease. If the leader doesn't refresh within this window,
/// the key expires and another instance can claim leadership.
const LEASE_TTL: Duration = Duration::from_secs(30);

/// How often the leader refreshes its lease. Must be well under LEASE_TTL.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// How soon the leader retries after a refresh attempt fails without a
/// verdict. The lease is still valid, so retrying is cheap and buys the
/// cluster time to recover before the TTL runs out.
const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// How often followers check whether the leader key has expired.
const CAMPAIGN_INTERVAL: Duration = Duration::from_secs(5);

/// Bound on every lease operation (one CAS, one create, one read). Left to
/// the library, a read waits the client's 10s request timeout and a CAS two
/// 5s context timeouts, leaving room for one retry per lease; this fits
/// several.
const LEASE_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive campaign errors before the loop re-ensures the KV bucket
/// (~30s at the campaign interval). Pure so the threshold is testable.
fn should_reensure(consecutive_errors: u32) -> bool {
    consecutive_errors >= 6
}

/// The lease this instance holds: the revision of its last write (the CAS
/// baseline for the next refresh) and when that write was sent. The key
/// expires `LEASE_TTL` after the server stored it, which is no earlier than
/// `sent_at + LEASE_TTL`, so leadership is known-safe until then whatever
/// happens to individual refresh requests in between.
#[derive(Clone, Copy, Debug)]
struct Lease {
    revision: u64,
    sent_at: Instant,
}

impl Lease {
    fn expires_at(&self) -> Instant {
        self.sent_at + LEASE_TTL
    }

    fn is_valid(&self, now: Instant) -> bool {
        now < self.expires_at()
    }

    /// Delay before the next refresh attempt: the regular interval while
    /// refreshes succeed, the short retry interval once one has failed, and
    /// never past the lease's own expiry.
    fn next_refresh_delay(&self, degraded: bool, now: Instant) -> Duration {
        let interval = if degraded {
            REFRESH_RETRY_INTERVAL
        } else {
            REFRESH_INTERVAL
        };
        interval.min(self.expires_at().saturating_duration_since(now))
    }
}

/// What a failed refresh means for the holder.
#[derive(Debug, PartialEq)]
enum RefreshFailure {
    /// The key's revision moved under us: another instance owns it, or our
    /// own earlier write landed after its request timed out. Settle by
    /// reading the key.
    Superseded,
    /// No verdict — the request timed out or the server could not serve it.
    /// The lease itself is untouched.
    Transient,
}

fn classify_refresh_failure(err: &UpdateError) -> RefreshFailure {
    match err.kind() {
        UpdateErrorKind::WrongLastRevision => RefreshFailure::Superseded,
        UpdateErrorKind::InvalidKey | UpdateErrorKind::TimedOut | UpdateErrorKind::Other => {
            RefreshFailure::Transient
        }
    }
}

/// Who holds the leader key, as far as a read can tell.
#[derive(Debug, Clone, Copy)]
enum KeyState {
    Vacant,
    /// Our own instance id at this revision.
    Ours(u64),
    Theirs,
}

/// The lease operations the election needs, behind a trait so the role
/// transitions can be exercised without a NATS server. Every operation is
/// bounded by `LEASE_OP_TIMEOUT`.
#[async_trait]
trait LeaseOps: Send + Sync {
    /// Compare-and-swap the key at `revision`, returning the new revision.
    async fn refresh(&self, revision: u64) -> Result<u64, UpdateError>;
    /// Claim a vacant key.
    async fn create(&self) -> Result<u64, CreateError>;
    async fn read(&self) -> Result<KeyState>;
    /// Delete the key if it is still at `revision`.
    async fn release(&self, revision: u64) -> Result<(), UpdateError>;
    /// Move the NATS client off the server it is attached to.
    async fn escape_server(&self);
}

/// `LeaseOps` over the real KV bucket.
struct KvLease {
    store: Store,
    client: async_nats::Client,
    value: Bytes,
}

impl KvLease {
    async fn bounded<T, E>(
        op: impl std::future::Future<Output = Result<T, E>>,
        timed_out: impl FnOnce() -> E,
    ) -> Result<T, E> {
        tokio::time::timeout(LEASE_OP_TIMEOUT, op)
            .await
            .unwrap_or_else(|_| Err(timed_out()))
    }
}

#[async_trait]
impl LeaseOps for KvLease {
    async fn refresh(&self, revision: u64) -> Result<u64, UpdateError> {
        Self::bounded(
            self.store.update(LEADER_KEY, self.value.clone(), revision),
            || UpdateError::from(UpdateErrorKind::TimedOut),
        )
        .await
    }

    async fn create(&self) -> Result<u64, CreateError> {
        Self::bounded(self.store.create(LEADER_KEY, self.value.clone()), || {
            CreateError::from(CreateErrorKind::Publish)
        })
        .await
    }

    async fn read(&self) -> Result<KeyState> {
        let entry = Self::bounded(self.store.entry(LEADER_KEY), || {
            EntryError::from(EntryErrorKind::TimedOut)
        })
        .await?;
        Ok(match entry {
            Some(entry) if entry.operation == Operation::Put => {
                if entry.value == self.value {
                    KeyState::Ours(entry.revision)
                } else {
                    KeyState::Theirs
                }
            }
            _ => KeyState::Vacant,
        })
    }

    async fn release(&self, revision: u64) -> Result<(), UpdateError> {
        Self::bounded(
            self.store
                .delete_expect_revision(LEADER_KEY, Some(revision)),
            || UpdateError::from(UpdateErrorKind::TimedOut),
        )
        .await
    }

    /// A lease operation against a replicated bucket on a healthy cluster is
    /// answered in milliseconds. One that times out or cannot be served
    /// means the server this client is attached to is stalled or cut off
    /// from quorum while still accepting clients — a state the client's own
    /// ping/pong liveness cannot see. Reconnecting redials a random one of
    /// the servers in `[nats] url`, so a cluster deployment lists every
    /// node there. Only while connected: with the connection already down
    /// the client is reconnecting on its own, and a queued request would
    /// only bounce the connection it eventually makes.
    async fn escape_server(&self) {
        if self.client.connection_state() != async_nats::connection::State::Connected {
            return;
        }
        if let Err(e) = self.client.force_reconnect().await {
            debug!("NATS reconnect request failed: {e}");
        }
    }
}

enum Role {
    Follower {
        /// Consecutive campaign ERRORS (not "held by another"). A run of
        /// them means the bucket itself is gone — e.g. recreated NATS
        /// cluster state — and campaigning against a dead handle would
        /// otherwise wedge this instance as a silent ingress-less follower.
        campaign_errors: u32,
    },
    Leader {
        lease: Lease,
        /// The last refresh attempt failed without a verdict; retry sooner.
        degraded: bool,
    },
}

/// Outcome of one campaign round.
enum Campaign {
    Role(Role),
    /// Errors have piled up: the bucket handle must be re-ensured.
    Reensure,
}

/// The election's role logic: this instance's identity and leadership flag,
/// independent of how the lease is stored.
struct Roles {
    instance_id: String,
    is_leader: Arc<AtomicBool>,
    leader_tx: watch::Sender<bool>,
}

impl Roles {
    /// One refresh round as leader. The attempt is bounded by the lease's
    /// expiry as well as the operation timeout, so this instance never acts
    /// as leader past the moment another one could claim the key.
    async fn hold(&self, ops: &dyn LeaseOps, lease: Lease) -> Role {
        let sent_at = Instant::now();
        if !lease.is_valid(sent_at) {
            warn!(
                instance = %self.instance_id,
                "Lost leadership: lease expired before it could be refreshed"
            );
            return self.demote();
        }

        match tokio::time::timeout_at(lease.expires_at(), ops.refresh(lease.revision)).await {
            Ok(Ok(revision)) => {
                debug!(instance = %self.instance_id, "Leader lease refreshed");
                Role::Leader {
                    lease: Lease { revision, sent_at },
                    degraded: false,
                }
            }
            Ok(Err(err)) => match classify_refresh_failure(&err) {
                RefreshFailure::Superseded => self.settle_superseded(ops, lease, &err).await,
                RefreshFailure::Transient => {
                    warn!(
                        instance = %self.instance_id,
                        lease_valid_for_secs = lease
                            .expires_at()
                            .saturating_duration_since(Instant::now())
                            .as_secs(),
                        "Leader lease refresh failed (CAS at {}); keeping the lease, \
                         reconnecting NATS and retrying: {err}",
                        lease.revision
                    );
                    ops.escape_server().await;
                    Role::Leader {
                        lease,
                        degraded: true,
                    }
                }
            },
            Err(_) => {
                warn!(
                    instance = %self.instance_id,
                    "Lost leadership: lease expired with a refresh unanswered (CAS at {})",
                    lease.revision
                );
                ops.escape_server().await;
                self.demote()
            }
        }
    }

    /// The CAS baseline moved. Either another instance took the key after
    /// our lease lapsed, or our own refresh landed after its request timed
    /// out. Read the key to tell which: our value means we still hold it at
    /// the new revision — resume from there, dated at the lease we already
    /// knew to be valid so expiry is never overestimated.
    async fn settle_superseded(&self, ops: &dyn LeaseOps, lease: Lease, err: &UpdateError) -> Role {
        match ops.read().await {
            Ok(KeyState::Ours(revision)) => {
                info!(
                    instance = %self.instance_id,
                    revision,
                    "Leader lease revision moved by our own delayed write; resuming"
                );
                Role::Leader {
                    lease: Lease {
                        revision,
                        sent_at: lease.sent_at,
                    },
                    degraded: true,
                }
            }
            Ok(KeyState::Theirs | KeyState::Vacant) => {
                warn!(
                    instance = %self.instance_id,
                    "Lost leadership: the key is no longer ours (CAS at {}): {err}",
                    lease.revision
                );
                self.demote()
            }
            Err(read_err) => {
                warn!(
                    instance = %self.instance_id,
                    "Leader lease refresh failed (CAS at {}) and the key could not be read; \
                     keeping the lease, reconnecting NATS and retrying: {err}; {read_err}",
                    lease.revision
                );
                ops.escape_server().await;
                Role::Leader {
                    lease,
                    degraded: true,
                }
            }
        }
    }

    /// One campaign round as follower.
    async fn campaign(&self, ops: &dyn LeaseOps, campaign_errors: u32) -> Campaign {
        let sent_at = Instant::now();
        match self.try_acquire(ops).await {
            Ok(Some(revision)) => {
                info!(instance = %self.instance_id, "Acquired leadership");
                self.set_leader(true);
                Campaign::Role(Role::Leader {
                    lease: Lease { revision, sent_at },
                    degraded: false,
                })
            }
            Ok(None) => {
                debug!(instance = %self.instance_id, "Another instance is leader");
                Campaign::Role(Role::Follower { campaign_errors: 0 })
            }
            Err(e) => {
                debug!(instance = %self.instance_id, "Campaign failed: {e}");
                ops.escape_server().await;
                let campaign_errors = campaign_errors + 1;
                if should_reensure(campaign_errors) {
                    Campaign::Reensure
                } else {
                    Campaign::Role(Role::Follower { campaign_errors })
                }
            }
        }
    }

    /// Attempt to claim leadership. `Ok(Some)` carries the revision of a
    /// write we just made — a lease dated by the caller; `Ok(None)` means
    /// another instance holds the key; `Err` means the server could not
    /// answer, which the caller counts rather than mistaking for a lost race.
    ///
    /// A key already carrying our own id (a write of ours that landed after
    /// its request timed out) is never adopted as-is: its age is unknown. It
    /// is refreshed with a CAS instead, so the lease we hold is one we dated.
    async fn try_acquire(&self, ops: &dyn LeaseOps) -> Result<Option<u64>> {
        match ops.read().await? {
            KeyState::Theirs => Ok(None),
            KeyState::Ours(revision) => match ops.refresh(revision).await {
                Ok(revision) => Ok(Some(revision)),
                Err(e) if classify_refresh_failure(&e) == RefreshFailure::Superseded => Ok(None),
                Err(e) => Err(e.into()),
            },
            KeyState::Vacant => match ops.create().await {
                Ok(revision) => Ok(Some(revision)),
                Err(e) if e.kind() == CreateErrorKind::AlreadyExists => Ok(None),
                Err(e) => Err(e.into()),
            },
        }
    }

    /// Voluntarily give up leadership (graceful shutdown). The delete is
    /// revision-fenced so a resign racing a lost lease can't evict the
    /// successor's key. If our own delayed write moved the revision, the key
    /// is re-read and released at the revision it actually has; a server
    /// that cannot answer is not waited on further — the lease lapses.
    async fn resign(&self, ops: &dyn LeaseOps, role: &Role) {
        let Role::Leader { lease, .. } = role else {
            return;
        };
        info!(instance = %self.instance_id, "Resigning leadership");
        let released = match ops.release(lease.revision).await {
            Err(e) if classify_refresh_failure(&e) == RefreshFailure::Superseded => {
                match ops.read().await {
                    Ok(KeyState::Ours(revision)) => ops.release(revision).await,
                    Ok(KeyState::Theirs | KeyState::Vacant) => Ok(()),
                    Err(read_err) => {
                        warn!("Failed to re-read leader key during resign: {read_err}");
                        Ok(())
                    }
                }
            }
            other => other,
        };
        if let Err(e) = released {
            warn!("Failed to delete leader key during resign: {e}");
        }
        self.set_leader(false);
    }

    fn demote(&self) -> Role {
        self.set_leader(false);
        Role::Follower { campaign_errors: 0 }
    }

    fn set_leader(&self, value: bool) {
        let was_leader = self.is_leader.swap(value, Ordering::Relaxed);
        if was_leader != value {
            let _ = self.leader_tx.send(value);
            if value {
                info!(instance = %self.instance_id, "This instance is now the LEADER");
            } else {
                info!(instance = %self.instance_id, "This instance is now a FOLLOWER");
            }
        }
    }
}

/// NATS KV-based leader election.
///
/// Only one instance in the cluster holds leadership at a time. The leader
/// maintains its lease by periodically updating the KV key. When the leader
/// stops refreshing (crash, shutdown, network partition), the TTL expires
/// and another instance takes over.
///
/// A refresh that fails without a verdict does not cost leadership: the
/// lease stays valid until its TTL, so the holder retries — and moves its
/// NATS connection to another server, since a lease operation that times
/// out against a healthy cluster points at the server it is attached to.
/// Leadership is dropped only when the key is seen in another instance's
/// hands or the lease's own expiry passes unrefreshed.
pub struct LeaderElection {
    client: async_nats::Client,
    jetstream: Context,
    /// Replica count for the election KV bucket. 1 on a single NATS server;
    /// 3 on a 3-node cluster so the lease survives a node loss.
    replicas: usize,
    roles: Roles,
    leader_rx: watch::Receiver<bool>,
}

impl LeaderElection {
    pub fn new(
        client: async_nats::Client,
        jetstream: Context,
        instance_id: String,
        replicas: usize,
    ) -> Self {
        let (leader_tx, leader_rx) = watch::channel(false);

        Self {
            client,
            jetstream,
            replicas,
            roles: Roles {
                instance_id,
                is_leader: Arc::new(AtomicBool::new(false)),
                leader_tx,
            },
            leader_rx,
        }
    }

    /// Whether this instance currently holds leadership.
    pub fn is_leader(&self) -> bool {
        self.roles.is_leader.load(Ordering::Relaxed)
    }

    /// Get a shared reference to the leadership flag.
    /// Useful for passing to subsystems that need to check leadership.
    pub fn is_leader_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.roles.is_leader)
    }

    /// Get a receiver that notifies on leadership changes.
    /// `true` = this instance became leader, `false` = lost leadership.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.leader_rx.clone()
    }

    /// Run the leader election loop. This never returns unless cancelled.
    pub async fn run(&self, cancel: CancellationToken) {
        let Some(mut ops) = self.ensure_lease_ops(&cancel).await else {
            return;
        };

        info!(instance = %self.roles.instance_id, "Leader election started");

        let mut role = Role::Follower { campaign_errors: 0 };

        loop {
            if cancel.is_cancelled() {
                self.roles.resign(&ops, &role).await;
                return;
            }

            role = match role {
                Role::Leader { lease, .. } => self.roles.hold(&ops, lease).await,
                Role::Follower { campaign_errors } => {
                    match self.roles.campaign(&ops, campaign_errors).await {
                        Campaign::Role(next) => next,
                        Campaign::Reensure => {
                            warn!(
                                instance = %self.roles.instance_id,
                                "Repeated campaign errors — re-ensuring the leader KV bucket"
                            );
                            match self.ensure_lease_ops(&cancel).await {
                                Some(fresh) => ops = fresh,
                                None => return,
                            }
                            Role::Follower { campaign_errors: 0 }
                        }
                    }
                }
            };

            let delay = match &role {
                Role::Leader { lease, degraded } => {
                    lease.next_refresh_delay(*degraded, Instant::now())
                }
                Role::Follower { .. } => CAMPAIGN_INTERVAL,
            };

            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => {
                    self.roles.resign(&ops, &role).await;
                    return;
                }
            }
        }
    }

    async fn ensure_lease_ops(&self, cancel: &CancellationToken) -> Option<KvLease> {
        let store = self.ensure_bucket_with_retry(cancel).await?;
        Some(KvLease {
            store,
            client: self.client.clone(),
            value: Bytes::from(self.roles.instance_id.clone().into_bytes()),
        })
    }

    /// Create or get the KV bucket for leader election.
    ///
    /// `create_key_value` re-creates the backing stream on every boot, and
    /// errors when a bucket already exists with a DIFFERENT config (e.g. the
    /// replica count mid-migration to a clustered NATS). That must not brick
    /// election — an instance stuck in the retry loop is a permanent
    /// ingress-less follower — so on create failure fall back to opening the
    /// existing bucket as-is.
    async fn ensure_bucket(&self) -> Result<Store> {
        let create_err = match self
            .jetstream
            .create_key_value(KvConfig {
                bucket: BUCKET_NAME.to_string(),
                max_value_size: 256,
                history: 1,
                max_age: LEASE_TTL,
                num_replicas: self.replicas,
                ..Default::default()
            })
            .await
        {
            Ok(store) => {
                info!(bucket = BUCKET_NAME, "Leader election KV bucket ready");
                return Ok(store);
            }
            Err(e) => e,
        };

        match self.jetstream.get_key_value(BUCKET_NAME).await {
            Ok(store) => {
                warn!(
                    bucket = BUCKET_NAME,
                    "Leader election KV bucket exists with a different config; opened as-is: \
                     {create_err}"
                );
                self.reconcile_bucket_replicas().await;
                Ok(store)
            }
            Err(_) => Err(eyre::eyre!(
                "Failed to create KV bucket '{}': {}",
                BUCKET_NAME,
                create_err
            )),
        }
    }

    /// Bring the bucket's backing-stream replica count in line with the
    /// configured value. The nats CLI refuses to edit KV-backing streams, so
    /// the hub owns this reconcile. Best-effort: election works on the bucket
    /// as-is either way.
    async fn reconcile_bucket_replicas(&self) {
        let stream_name = format!("KV_{BUCKET_NAME}");
        let stream = match self.jetstream.get_stream(&stream_name).await {
            Ok(stream) => stream,
            Err(e) => {
                debug!("Leader KV stream unavailable for replica reconcile: {e}");
                return;
            }
        };
        let current = stream.cached_info().config.num_replicas;
        if current == self.replicas {
            return;
        }
        let mut config = stream.cached_info().config.clone();
        config.num_replicas = self.replicas;
        match self.jetstream.update_stream(&config).await {
            Ok(_) => info!(
                stream = %stream_name,
                from = current,
                to = self.replicas,
                "Leader KV bucket replicas updated"
            ),
            Err(e) => {
                warn!(stream = %stream_name, "Updating leader KV bucket replicas failed: {e}")
            }
        }
    }

    /// Obtain the leader KV bucket, retrying until it succeeds or the token
    /// cancels. Fails CLOSED: while the bucket is unreachable this instance
    /// stays a follower (`set_leader(false)`) and runs no singletons, rather
    /// than assuming leadership — so a degraded NATS can never make every
    /// instance act as leader and double-run scheduler/heartbeat/Slack. It
    /// self-heals once NATS recovers. Returns `None` only on cancellation.
    async fn ensure_bucket_with_retry(&self, cancel: &CancellationToken) -> Option<Store> {
        let mut warned = false;
        loop {
            if cancel.is_cancelled() {
                return None;
            }
            match self.ensure_bucket().await {
                Ok(store) => return Some(store),
                Err(e) => {
                    if warned {
                        debug!(instance = %self.roles.instance_id, "Leader KV bucket still unavailable: {e}");
                    } else {
                        warn!(
                            instance = %self.roles.instance_id,
                            "Leader KV bucket unavailable, staying follower (no singletons): {e}"
                        );
                        warned = true;
                    }
                    self.roles.set_leader(false);
                    tokio::select! {
                        _ = tokio::time::sleep(CAMPAIGN_INTERVAL) => {}
                        _ = cancel.cancelled() => return None,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    fn lease_at(sent_at: Instant) -> Lease {
        Lease {
            revision: 7,
            sent_at,
        }
    }

    fn roles() -> (Roles, watch::Receiver<bool>) {
        let (leader_tx, leader_rx) = watch::channel(false);
        (
            Roles {
                instance_id: "hub-a".into(),
                is_leader: Arc::new(AtomicBool::new(false)),
                leader_tx,
            },
            leader_rx,
        )
    }

    /// Scripted lease operations: each call pops the next queued answer.
    #[derive(Default)]
    struct FakeOps {
        refreshes: Mutex<VecDeque<Result<u64, UpdateErrorKind>>>,
        creates: Mutex<VecDeque<Result<u64, CreateErrorKind>>>,
        reads: Mutex<VecDeque<Result<KeyState, String>>>,
        refresh_revisions: Mutex<Vec<u64>>,
        escapes: AtomicUsize,
        release_answers: Mutex<VecDeque<Result<(), UpdateErrorKind>>>,
        releases: Mutex<Vec<u64>>,
    }

    impl FakeOps {
        fn refreshing(
            self,
            answers: impl IntoIterator<Item = Result<u64, UpdateErrorKind>>,
        ) -> Self {
            self.refreshes.lock().extend(answers);
            self
        }

        fn creating(self, answers: impl IntoIterator<Item = Result<u64, CreateErrorKind>>) -> Self {
            self.creates.lock().extend(answers);
            self
        }

        fn reading(
            self,
            answers: impl IntoIterator<Item = Result<KeyState, &'static str>>,
        ) -> Self {
            self.reads
                .lock()
                .extend(answers.into_iter().map(|a| a.map_err(str::to_owned)));
            self
        }

        fn releasing(self, answers: impl IntoIterator<Item = Result<(), UpdateErrorKind>>) -> Self {
            self.release_answers.lock().extend(answers);
            self
        }

        fn escapes(&self) -> usize {
            self.escapes.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl LeaseOps for FakeOps {
        async fn refresh(&self, revision: u64) -> Result<u64, UpdateError> {
            self.refresh_revisions.lock().push(revision);
            self.refreshes
                .lock()
                .pop_front()
                .expect("unscripted refresh")
                .map_err(UpdateError::from)
        }

        async fn create(&self) -> Result<u64, CreateError> {
            self.creates
                .lock()
                .pop_front()
                .expect("unscripted create")
                .map_err(CreateError::from)
        }

        async fn read(&self) -> Result<KeyState> {
            self.reads
                .lock()
                .pop_front()
                .expect("unscripted read")
                .map_err(|e| eyre::eyre!(e))
        }

        async fn release(&self, revision: u64) -> Result<(), UpdateError> {
            self.releases.lock().push(revision);
            self.release_answers
                .lock()
                .pop_front()
                .unwrap_or(Ok(()))
                .map_err(UpdateError::from)
        }

        async fn escape_server(&self) {
            self.escapes.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn leader(role: &Role) -> Option<(Lease, bool)> {
        match role {
            Role::Leader { lease, degraded } => Some((*lease, *degraded)),
            Role::Follower { .. } => None,
        }
    }

    #[test]
    fn reensure_fires_only_after_sustained_errors() {
        assert!(!should_reensure(0));
        assert!(!should_reensure(5));
        assert!(should_reensure(6));
        assert!(should_reensure(7));
    }

    #[test]
    fn lease_is_valid_until_its_ttl_from_the_send_time() {
        let sent = Instant::now();
        let lease = lease_at(sent);
        assert!(lease.is_valid(sent));
        assert!(lease.is_valid(sent + LEASE_TTL - Duration::from_millis(1)));
        assert!(!lease.is_valid(sent + LEASE_TTL));
    }

    #[test]
    fn healthy_leader_refreshes_on_the_regular_interval() {
        let sent = Instant::now();
        assert_eq!(
            lease_at(sent).next_refresh_delay(false, sent),
            REFRESH_INTERVAL
        );
    }

    #[test]
    fn degraded_leader_retries_quickly_but_never_past_expiry() {
        let sent = Instant::now();
        let lease = lease_at(sent);
        assert_eq!(lease.next_refresh_delay(true, sent), REFRESH_RETRY_INTERVAL);
        let late = sent + LEASE_TTL - Duration::from_millis(500);
        assert_eq!(
            lease.next_refresh_delay(true, late),
            Duration::from_millis(500)
        );
        assert_eq!(
            lease.next_refresh_delay(false, sent + LEASE_TTL),
            Duration::ZERO
        );
    }

    #[test]
    fn a_moved_revision_is_the_only_definitive_refresh_failure() {
        assert_eq!(
            classify_refresh_failure(&UpdateError::from(UpdateErrorKind::WrongLastRevision)),
            RefreshFailure::Superseded
        );
        for kind in [
            UpdateErrorKind::TimedOut,
            UpdateErrorKind::Other,
            UpdateErrorKind::InvalidKey,
        ] {
            assert_eq!(
                classify_refresh_failure(&UpdateError::from(kind)),
                RefreshFailure::Transient
            );
        }
    }

    #[test]
    fn two_full_refresh_attempts_fit_inside_one_lease() {
        let two_attempts =
            REFRESH_INTERVAL + LEASE_OP_TIMEOUT + REFRESH_RETRY_INTERVAL + LEASE_OP_TIMEOUT;
        assert!(two_attempts < LEASE_TTL);
    }

    #[tokio::test]
    async fn a_transient_refresh_failure_keeps_the_lease_and_escapes_the_server() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default().refreshing([Err(UpdateErrorKind::TimedOut)]);
        let lease = lease_at(Instant::now());

        let next = roles.hold(&ops, lease).await;

        let (kept, degraded) = leader(&next).expect("still leader");
        assert_eq!(kept.revision, lease.revision);
        assert_eq!(kept.sent_at, lease.sent_at);
        assert!(degraded);
        assert!(roles.is_leader.load(Ordering::Relaxed));
        assert_eq!(ops.escapes(), 1);
    }

    #[tokio::test]
    async fn a_successful_refresh_dates_the_lease_at_the_send_time() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default().refreshing([Ok(8)]);
        let old = lease_at(Instant::now() - Duration::from_secs(10));

        let before = Instant::now();
        let next = roles.hold(&ops, old).await;

        let (fresh, degraded) = leader(&next).expect("still leader");
        assert_eq!(fresh.revision, 8);
        assert!(fresh.sent_at >= before);
        assert!(!degraded);
        assert_eq!(*ops.refresh_revisions.lock(), vec![7]);
        assert_eq!(ops.escapes(), 0);
    }

    #[tokio::test]
    async fn an_expired_lease_is_dropped_without_touching_the_key() {
        let (roles, mut rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default();
        let expired = lease_at(Instant::now() - LEASE_TTL);

        let next = roles.hold(&ops, expired).await;

        assert!(leader(&next).is_none());
        assert!(!roles.is_leader.load(Ordering::Relaxed));
        assert!(rx.has_changed().unwrap());
        assert!(!*rx.borrow_and_update());
        assert!(ops.refresh_revisions.lock().is_empty());
    }

    #[tokio::test]
    async fn our_own_delayed_write_is_resumed_at_the_old_lease_date() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default()
            .refreshing([Err(UpdateErrorKind::WrongLastRevision)])
            .reading([Ok(KeyState::Ours(9))]);
        let lease = lease_at(Instant::now() - Duration::from_secs(10));

        let next = roles.hold(&ops, lease).await;

        let (resumed, degraded) = leader(&next).expect("still leader");
        assert_eq!(resumed.revision, 9);
        assert_eq!(resumed.sent_at, lease.sent_at);
        assert!(degraded);
        assert_eq!(ops.escapes(), 0);
    }

    #[tokio::test]
    async fn a_key_in_other_hands_ends_leadership() {
        for state in [KeyState::Theirs, KeyState::Vacant] {
            let (roles, _rx) = roles();
            roles.set_leader(true);
            let ops = FakeOps::default()
                .refreshing([Err(UpdateErrorKind::WrongLastRevision)])
                .reading([Ok(state)]);

            let next = roles.hold(&ops, lease_at(Instant::now())).await;

            assert!(leader(&next).is_none());
            assert!(!roles.is_leader.load(Ordering::Relaxed));
        }
    }

    #[tokio::test]
    async fn an_unreadable_key_after_a_moved_revision_keeps_the_lease() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default()
            .refreshing([Err(UpdateErrorKind::WrongLastRevision)])
            .reading([Err("timed out")]);
        let lease = lease_at(Instant::now());

        let next = roles.hold(&ops, lease).await;

        let (kept, degraded) = leader(&next).expect("still leader");
        assert_eq!(kept.revision, lease.revision);
        assert!(degraded);
        assert_eq!(ops.escapes(), 1);
    }

    #[tokio::test]
    async fn a_vacant_key_is_claimed_and_dated_now() {
        let (roles, mut rx) = roles();
        let ops = FakeOps::default()
            .reading([Ok(KeyState::Vacant)])
            .creating([Ok(3)]);

        let before = Instant::now();
        let Campaign::Role(next) = roles.campaign(&ops, 0).await else {
            panic!("no re-ensure on success");
        };

        let (lease, degraded) = leader(&next).expect("acquired");
        assert_eq!(lease.revision, 3);
        assert!(lease.sent_at >= before);
        assert!(!degraded);
        assert!(*rx.borrow_and_update());
    }

    #[tokio::test]
    async fn our_own_stale_key_is_refreshed_rather_than_adopted() {
        let (roles, _rx) = roles();
        let ops = FakeOps::default()
            .reading([Ok(KeyState::Ours(4))])
            .refreshing([Ok(5)]);

        let Campaign::Role(next) = roles.campaign(&ops, 0).await else {
            panic!("no re-ensure on success");
        };

        let (lease, _) = leader(&next).expect("acquired");
        assert_eq!(lease.revision, 5);
        assert_eq!(*ops.refresh_revisions.lock(), vec![4]);
    }

    #[tokio::test]
    async fn losing_the_race_is_not_an_error() {
        let (roles, _rx) = roles();
        let ops = FakeOps::default()
            .reading([
                Ok(KeyState::Theirs),
                Ok(KeyState::Vacant),
                Ok(KeyState::Ours(4)),
            ])
            .creating([Err(CreateErrorKind::AlreadyExists)])
            .refreshing([Err(UpdateErrorKind::WrongLastRevision)]);

        for _ in 0..3 {
            match roles.campaign(&ops, 5).await {
                Campaign::Role(Role::Follower { campaign_errors }) => {
                    assert_eq!(campaign_errors, 0)
                }
                _ => panic!("a lost race must leave the follower settled"),
            }
        }
        assert!(!roles.is_leader.load(Ordering::Relaxed));
        assert_eq!(ops.escapes(), 0);
    }

    #[tokio::test]
    async fn campaign_errors_escape_the_server_and_eventually_reensure() {
        let (roles, _rx) = roles();
        let ops = FakeOps::default()
            .reading([Err("timed out"), Ok(KeyState::Vacant)])
            .creating([Err(CreateErrorKind::Publish)]);

        let Campaign::Role(Role::Follower { campaign_errors }) = roles.campaign(&ops, 0).await
        else {
            panic!("a server error keeps the follower campaigning");
        };
        assert_eq!(campaign_errors, 1);
        assert_eq!(ops.escapes(), 1);

        assert!(matches!(roles.campaign(&ops, 5).await, Campaign::Reensure));
        assert_eq!(ops.escapes(), 2);
    }

    /// Lease operations that never answer — a server that accepts requests
    /// and says nothing.
    struct SilentOps;

    #[async_trait]
    impl LeaseOps for SilentOps {
        async fn refresh(&self, _revision: u64) -> Result<u64, UpdateError> {
            std::future::pending().await
        }
        async fn create(&self) -> Result<u64, CreateError> {
            std::future::pending().await
        }
        async fn read(&self) -> Result<KeyState> {
            std::future::pending().await
        }
        async fn release(&self, _revision: u64) -> Result<(), UpdateError> {
            std::future::pending().await
        }
        async fn escape_server(&self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_still_unanswered_at_expiry_ends_leadership() {
        let (roles, mut rx) = roles();
        roles.set_leader(true);
        let lease = lease_at(Instant::now() - LEASE_TTL + Duration::from_millis(50));

        let next = roles.hold(&SilentOps, lease).await;

        assert!(leader(&next).is_none());
        assert!(!*rx.borrow_and_update());
        assert!(Instant::now() >= lease.expires_at());
    }

    #[tokio::test]
    async fn an_unanswered_refresh_of_our_own_stale_key_is_a_campaign_error() {
        let (roles, _rx) = roles();
        let ops = FakeOps::default()
            .reading([Ok(KeyState::Ours(4))])
            .refreshing([Err(UpdateErrorKind::TimedOut)]);

        let Campaign::Role(Role::Follower { campaign_errors }) = roles.campaign(&ops, 0).await
        else {
            panic!("a server error keeps the follower campaigning");
        };
        assert_eq!(campaign_errors, 1);
        assert_eq!(ops.escapes(), 1);
        assert!(!roles.is_leader.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn resign_follows_our_own_moved_revision() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default()
            .releasing([Err(UpdateErrorKind::WrongLastRevision)])
            .reading([Ok(KeyState::Ours(9))]);

        roles
            .resign(
                &ops,
                &Role::Leader {
                    lease: lease_at(Instant::now()),
                    degraded: false,
                },
            )
            .await;

        assert_eq!(*ops.releases.lock(), vec![7, 9]);
        assert!(!roles.is_leader.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn resign_does_not_wait_on_a_server_that_cannot_answer() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default().releasing([Err(UpdateErrorKind::TimedOut)]);

        roles
            .resign(
                &ops,
                &Role::Leader {
                    lease: lease_at(Instant::now()),
                    degraded: false,
                },
            )
            .await;

        assert_eq!(*ops.releases.lock(), vec![7]);
        assert!(ops.reads.lock().is_empty());
        assert!(!roles.is_leader.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn resign_releases_the_lease_and_demotes() {
        let (roles, _rx) = roles();
        roles.set_leader(true);
        let ops = FakeOps::default();

        roles
            .resign(
                &ops,
                &Role::Leader {
                    lease: lease_at(Instant::now()),
                    degraded: false,
                },
            )
            .await;

        assert_eq!(*ops.releases.lock(), vec![7]);
        assert!(!roles.is_leader.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn resign_as_follower_touches_nothing() {
        let (roles, _rx) = roles();
        let ops = FakeOps::default();

        roles
            .resign(&ops, &Role::Follower { campaign_errors: 0 })
            .await;

        assert!(ops.releases.lock().is_empty());
    }
}
