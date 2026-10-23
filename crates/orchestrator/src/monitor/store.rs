//! Durability state for monitors: the per-monitor catch-up cursor of a
//! channel, the observation state of a source, and the per-(monitor, event)
//! dispatch gate they both converge on.
//!
//! The dispatch table is both the idempotency gate and the durable work queue.
//! A claim persists the event payload, so an incomplete dispatch is recovered
//! directly (no channel re-read, no second poll) and a cursor may advance
//! freely without ever stranding work. Monitor *definitions* live in
//! `workflow_defs` (see [`crate::workflow_store`]); only runtime state lives
//! here.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};

use workforce_common::postgres::{PostgresConfig, PostgresError, PostgresPool};

use super::event::MonitorEvent;
use super::observe::ObservedState;
use crate::workflow_store::SYSTEM_OWNER;

fn owner_col(owner: Option<&str>) -> &str {
    owner.unwrap_or(SYSTEM_OWNER)
}

fn monitor_key(row: &tokio_postgres::Row) -> MonitorKey {
    let owner: String = row.get("owner");
    MonitorKey {
        monitor_id: row.get("monitor_id"),
        owner: (owner != SYSTEM_OWNER).then_some(owner),
    }
}

/// Identifies one dispatch: a single monitor acting on a single message.
#[derive(Debug, Clone)]
pub struct DispatchKey {
    pub monitor_id: String,
    pub owner: Option<String>,
    pub surface: String,
    pub channel: String,
    pub message_id: String,
}

/// Outcome of an atomic [`MonitorStore::claim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// This caller owns the dispatch and must perform the action.
    Won,
    /// Already completed by an earlier dispatch — skip.
    AlreadyDone,
    /// Freshly claimed elsewhere and still in flight — skip.
    InFlight,
}

/// An incomplete dispatch returned by [`MonitorStore::recover`], carrying the
/// stored event so the action can be re-run without re-reading its source.
#[derive(Debug, Clone)]
pub struct RecoverableDispatch {
    pub key: DispatchKey,
    pub event: MonitorEvent,
}

/// What a source monitor's last poll left behind.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    /// What the last *successful* poll saw; `None` until one has.
    pub observed: Option<ObservedState>,
    pub polled_at: Option<DateTime<Utc>>,
    pub next_due_at: DateTime<Utc>,
    pub error: Option<String>,
    pub failures: u32,
}

impl SourceState {
    /// Whether a poll is owed at `now`.
    pub fn due_at(&self, now: DateTime<Utc>) -> bool {
        self.next_due_at <= now
    }
}

/// A source monitor's poll, as a listing shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PollStatus {
    pub polled_at: Option<DateTime<Utc>>,
    pub next_due_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub failures: u32,
}

impl From<&SourceState> for PollStatus {
    fn from(state: &SourceState) -> Self {
        Self {
            polled_at: state.polled_at,
            next_due_at: state.next_due_at,
            error: state.error.clone(),
            failures: state.failures,
        }
    }
}

/// One recent dispatch, for the monitor activity panel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DispatchActivity {
    pub message_id: String,
    pub channel: String,
    pub text: String,
    pub claimed_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub instance_id: Option<String>,
    pub error: Option<String>,
}

/// Identifies one monitor definition: bare id plus owner namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MonitorKey {
    pub monitor_id: String,
    pub owner: Option<String>,
}

impl MonitorKey {
    /// One monitor: its bare id in an owner's namespace (`None` = the install's).
    pub fn new(monitor_id: &str, owner: Option<&str>) -> Self {
        Self {
            monitor_id: monitor_id.to_string(),
            owner: owner.map(str::to_string),
        }
    }
}

/// A monitor whose watching agent's bot cannot read its channel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelBlock {
    #[serde(skip)]
    pub key: MonitorKey,
    pub agent: String,
    pub surface: String,
    pub channel: String,
    pub error: String,
    pub since: DateTime<Utc>,
}

/// Postgres-backed monitor durability store.
pub struct MonitorStore {
    pool: PostgresPool,
}

impl MonitorStore {
    /// Telemetry-pinned pool (the monitor tables live in that schema).
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool(config)
    }

    /// Ensure the tables exist, then return a ready store.
    pub async fn new(pool: PostgresPool) -> Result<Self> {
        workforce_common::telemetry_postgres::run_telemetry_migrations(&pool)
            .await
            .context("ensuring monitor tables exist")?;
        Ok(Self { pool })
    }

    /// Atomically claim `(monitor, message)`, persisting the event payload.
    /// A fresh claim or a stale-incomplete re-claim returns [`Claim::Won`];
    /// otherwise [`Claim::AlreadyDone`] or [`Claim::InFlight`].
    pub async fn claim(
        &self,
        key: &DispatchKey,
        ev: &MonitorEvent,
        stale_after: Duration,
    ) -> Result<Claim> {
        let conn = self.pool.get().await?;
        let owner = owner_col(key.owner.as_deref());
        let payload = serde_json::to_value(ev)?;
        let workspace = ev.message().map_or("", |m| m.workspace.as_str());
        let stale_secs = stale_after.as_secs() as i64;

        let won = conn
            .query_opt(
                "INSERT INTO monitor_dispatches \
                 (monitor_id, owner, surface, workspace, channel, message_id, payload, claimed_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
                 ON CONFLICT (monitor_id, owner, surface, channel, message_id) DO UPDATE \
                   SET claimed_at = now(), error = NULL \
                   WHERE monitor_dispatches.completed_at IS NULL \
                     AND monitor_dispatches.claimed_at < now() - ($8::bigint * interval '1 second') \
                 RETURNING 1::int AS ok",
                &[
                    &key.monitor_id,
                    &owner,
                    &key.surface,
                    &workspace,
                    &key.channel,
                    &key.message_id,
                    &payload,
                    &stale_secs,
                ],
            )
            .await?;
        if won.is_some() {
            return Ok(Claim::Won);
        }

        // Conflict whose update predicate did not match: either completed, or
        // freshly claimed and still in flight.
        let existing = conn
            .query_opt(
                "SELECT completed_at FROM monitor_dispatches \
                 WHERE monitor_id = $1 AND owner = $2 AND surface = $3 \
                   AND channel = $4 AND message_id = $5",
                &[
                    &key.monitor_id,
                    &owner,
                    &key.surface,
                    &key.channel,
                    &key.message_id,
                ],
            )
            .await?;
        match existing {
            Some(r) if r.get::<_, Option<DateTime<Utc>>>("completed_at").is_some() => {
                Ok(Claim::AlreadyDone)
            }
            _ => Ok(Claim::InFlight),
        }
    }

    /// Mark a won dispatch completed.
    pub async fn complete(&self, key: &DispatchKey, instance_id: Option<&str>) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE monitor_dispatches \
             SET completed_at = now(), instance_id = $6, error = NULL \
             WHERE monitor_id = $1 AND owner = $2 AND surface = $3 \
               AND channel = $4 AND message_id = $5",
            &[
                &key.monitor_id,
                &owner_col(key.owner.as_deref()),
                &key.surface,
                &key.channel,
                &key.message_id,
                &instance_id,
            ],
        )
        .await?;
        Ok(())
    }

    /// Record an action failure. `terminal` (e.g. the workflow no longer exists)
    /// marks the dispatch completed so it never retries; a transient failure
    /// leaves it incomplete for the recovery pass.
    pub async fn fail(&self, key: &DispatchKey, error: &str, terminal: bool) -> Result<()> {
        let conn = self.pool.get().await?;
        let sql = if terminal {
            "UPDATE monitor_dispatches SET completed_at = now(), error = $6 \
             WHERE monitor_id = $1 AND owner = $2 AND surface = $3 \
               AND channel = $4 AND message_id = $5"
        } else {
            "UPDATE monitor_dispatches SET error = $6 \
             WHERE monitor_id = $1 AND owner = $2 AND surface = $3 \
               AND channel = $4 AND message_id = $5"
        };
        conn.execute(
            sql,
            &[
                &key.monitor_id,
                &owner_col(key.owner.as_deref()),
                &key.surface,
                &key.channel,
                &key.message_id,
                &error,
            ],
        )
        .await?;
        Ok(())
    }

    /// Hand a reclaimed-but-untouched dispatch straight back: the next recovery
    /// pass (any engine's) sees it as stale at once instead of after
    /// `stale_after`.
    pub async fn release(&self, key: &DispatchKey) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE monitor_dispatches SET claimed_at = to_timestamp(0) \
             WHERE monitor_id = $1 AND owner = $2 AND surface = $3 \
               AND channel = $4 AND message_id = $5 AND completed_at IS NULL",
            &[
                &key.monitor_id,
                &owner_col(key.owner.as_deref()),
                &key.surface,
                &key.channel,
                &key.message_id,
            ],
        )
        .await?;
        Ok(())
    }

    /// Reclaim incomplete dispatches older than `stale_after` and return them
    /// with their stored payload for direct re-dispatch.
    pub async fn recover(
        &self,
        stale_after: Duration,
        limit: i64,
    ) -> Result<Vec<RecoverableDispatch>> {
        let conn = self.pool.get().await?;
        let stale_secs = stale_after.as_secs() as i64;
        let rows = conn
            .query(
                "WITH stale AS ( \
                   SELECT monitor_id, owner, surface, channel, message_id \
                   FROM monitor_dispatches \
                   WHERE completed_at IS NULL \
                     AND claimed_at < now() - ($1::bigint * interval '1 second') \
                   ORDER BY claimed_at LIMIT $2 FOR UPDATE SKIP LOCKED) \
                 UPDATE monitor_dispatches d SET claimed_at = now() FROM stale s \
                 WHERE d.monitor_id = s.monitor_id AND d.owner = s.owner \
                   AND d.surface = s.surface AND d.channel = s.channel \
                   AND d.message_id = s.message_id \
                 RETURNING d.monitor_id, d.owner, d.surface, d.channel, d.message_id, d.payload",
                &[&stale_secs, &limit],
            )
            .await?;
        let mut recovered = Vec::with_capacity(rows.len());
        for r in &rows {
            let stored: String = r.get("owner");
            let key = DispatchKey {
                monitor_id: r.get("monitor_id"),
                owner: (stored != SYSTEM_OWNER).then_some(stored),
                surface: r.get("surface"),
                channel: r.get("channel"),
                message_id: r.get("message_id"),
            };
            match serde_json::from_value::<MonitorEvent>(r.get("payload")) {
                Ok(event) => recovered.push(RecoverableDispatch { key, event }),
                Err(e) => {
                    tracing::warn!(monitor = %key.monitor_id, message = %key.message_id, error = %e,
                        "monitor dispatch payload is unreadable; closing it");
                    self.fail(&key, &format!("unreadable payload: {e}"), true)
                        .await?;
                }
            }
        }
        Ok(recovered)
    }

    /// Where each monitor watching `(surface, channel)` has read to.
    pub async fn channel_cursors(
        &self,
        surface: &str,
        channel: &str,
    ) -> Result<HashMap<MonitorKey, String>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT monitor_id, owner, last_swept_ts FROM monitor_cursors \
                 WHERE surface = $1 AND channel = $2",
                &[&surface, &channel],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| (monitor_key(r), r.get("last_swept_ts")))
            .collect())
    }

    /// Point `key`'s catch-up at `bindings` alone: a binding with no cursor
    /// starts at `ts`, an existing one keeps its place unless `overwrite` (a
    /// resume), and a binding the monitor no longer has is dropped. The prune
    /// and the upsert address disjoint rows, so both run in one statement.
    pub async fn set_cursors(
        &self,
        key: &MonitorKey,
        bindings: &[(String, String)],
        ts: &str,
        overwrite: bool,
    ) -> Result<()> {
        let (surfaces, channels): (Vec<&str>, Vec<&str>) = bindings
            .iter()
            .map(|(surface, channel)| (surface.as_str(), channel.as_str()))
            .unzip();
        let conn = self.pool.get().await?;
        conn.execute(
            "WITH bound AS (SELECT * FROM UNNEST($3::text[], $4::text[]) AS b(surface, channel)), \
                  pruned AS ( \
                    DELETE FROM monitor_cursors c \
                     WHERE c.monitor_id = $1 AND c.owner = $2 \
                       AND NOT EXISTS (SELECT 1 FROM bound b \
                                        WHERE b.surface = c.surface AND b.channel = c.channel)) \
             INSERT INTO monitor_cursors (monitor_id, owner, surface, channel, last_swept_ts) \
             SELECT $1, $2, b.surface, b.channel, $5 FROM bound b \
             ON CONFLICT (monitor_id, owner, surface, channel) DO UPDATE \
                SET last_swept_ts = EXCLUDED.last_swept_ts, updated_at = now() \
              WHERE $6::bool",
            &[
                &key.monitor_id,
                &owner_col(key.owner.as_deref()),
                &surfaces,
                &channels,
                &ts,
                &overwrite,
            ],
        )
        .await?;
        Ok(())
    }

    /// Advance every `key`'s cursor on `(surface, channel)` to `ts`, never
    /// backwards. A monitor with no row yet is created at `ts`; an empty
    /// `keys` is a no-op.
    pub async fn advance_cursors(
        &self,
        keys: &[MonitorKey],
        surface: &str,
        channel: &str,
        ts: &str,
    ) -> Result<()> {
        let (ids, owners): (Vec<&str>, Vec<&str>) = keys
            .iter()
            .map(|key| (key.monitor_id.as_str(), owner_col(key.owner.as_deref())))
            .unzip();
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO monitor_cursors (monitor_id, owner, surface, channel, last_swept_ts) \
             SELECT k.monitor_id, k.owner, $3, $4, $5 \
               FROM UNNEST($1::text[], $2::text[]) AS k(monitor_id, owner) \
             ON CONFLICT (monitor_id, owner, surface, channel) DO UPDATE \
                SET last_swept_ts = EXCLUDED.last_swept_ts, updated_at = now() \
              WHERE EXCLUDED.last_swept_ts::double precision \
                    > monitor_cursors.last_swept_ts::double precision",
            &[&ids, &owners, &surface, &channel, &ts],
        )
        .await?;
        Ok(())
    }

    /// Drop a monitor's cursors when the monitor itself goes away.
    pub async fn forget_cursors(&self, key: &MonitorKey) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM monitor_cursors WHERE monitor_id = $1 AND owner = $2",
            &[&key.monitor_id, &owner_col(key.owner.as_deref())],
        )
        .await?;
        Ok(())
    }

    /// Recent dispatches for one monitor, newest first (activity panel).
    pub async fn recent_activity(
        &self,
        monitor_id: &str,
        owner: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DispatchActivity>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT message_id, channel, payload->>'text' AS text, claimed_at, \
                        completed_at, instance_id, error \
                 FROM monitor_dispatches WHERE monitor_id = $1 AND owner = $2 \
                 ORDER BY claimed_at DESC LIMIT $3",
                &[&monitor_id, &owner_col(owner), &limit],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| DispatchActivity {
                message_id: r.get("message_id"),
                channel: r.get("channel"),
                text: r.get::<_, Option<String>>("text").unwrap_or_default(),
                claimed_at: r.get("claimed_at"),
                completed_at: r.get("completed_at"),
                instance_id: r.get("instance_id"),
                error: r.get("error"),
            })
            .collect())
    }

    /// Record that `agent`'s bot cannot read the channel `key` is bound to.
    /// Returns `true` when this is a fresh block — the first sighting, or the
    /// monitor now blocked on a different `(agent, surface, channel)` — which is
    /// the moment to tell the owner. A repeat sighting keeps `since`.
    pub async fn block(
        &self,
        key: &MonitorKey,
        agent: &str,
        surface: &str,
        channel: &str,
        error: &str,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "INSERT INTO monitor_channel_blocks \
                 (monitor_id, owner, agent_id, surface, channel, error, blocked_since) \
                 VALUES ($1, $2, $3, $4, $5, $6, now()) \
                 ON CONFLICT (monitor_id, owner) DO UPDATE SET \
                   agent_id = EXCLUDED.agent_id, surface = EXCLUDED.surface, \
                   channel = EXCLUDED.channel, error = EXCLUDED.error, \
                   blocked_since = CASE \
                     WHEN monitor_channel_blocks.agent_id = EXCLUDED.agent_id \
                      AND monitor_channel_blocks.surface = EXCLUDED.surface \
                      AND monitor_channel_blocks.channel = EXCLUDED.channel \
                     THEN monitor_channel_blocks.blocked_since \
                     ELSE EXCLUDED.blocked_since END \
                 RETURNING blocked_since = now() AS fresh",
                &[
                    &key.monitor_id,
                    &owner_col(key.owner.as_deref()),
                    &agent,
                    &surface,
                    &channel,
                    &error,
                ],
            )
            .await?;
        Ok(row.get("fresh"))
    }

    /// Clear every block `agent` holds on `(surface, channel)` — the channel
    /// became readable — returning the monitors that were blocked.
    pub async fn unblock_channel(
        &self,
        agent: &str,
        surface: &str,
        channel: &str,
    ) -> Result<Vec<MonitorKey>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "DELETE FROM monitor_channel_blocks \
                 WHERE agent_id = $1 AND surface = $2 AND channel = $3 \
                 RETURNING monitor_id, owner",
                &[&agent, &surface, &channel],
            )
            .await?;
        Ok(rows.iter().map(monitor_key).collect())
    }

    /// Drop a monitor's block when the monitor itself goes away.
    pub async fn forget_block(&self, key: &MonitorKey) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM monitor_channel_blocks WHERE monitor_id = $1 AND owner = $2",
            &[&key.monitor_id, &owner_col(key.owner.as_deref())],
        )
        .await?;
        Ok(())
    }

    /// Every current block, for status listings.
    pub async fn blocks(&self) -> Result<Vec<ChannelBlock>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT monitor_id, owner, agent_id, surface, channel, error, blocked_since \
                 FROM monitor_channel_blocks",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| ChannelBlock {
                key: monitor_key(r),
                agent: r.get("agent_id"),
                surface: r.get("surface"),
                channel: r.get("channel"),
                error: r.get("error"),
                since: r.get("blocked_since"),
            })
            .collect())
    }

    /// What each of `keys` left after its last poll; a monitor never polled
    /// has no entry.
    pub async fn source_states(
        &self,
        keys: &[MonitorKey],
    ) -> Result<HashMap<MonitorKey, SourceState>> {
        let (ids, owners): (Vec<&str>, Vec<&str>) = keys
            .iter()
            .map(|key| (key.monitor_id.as_str(), owner_col(key.owner.as_deref())))
            .unzip();
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT s.monitor_id, s.owner, s.observed, s.polled_at, s.next_due_at, \
                        s.error, s.failures \
                   FROM monitor_source_state s \
                   JOIN UNNEST($1::text[], $2::text[]) AS k(monitor_id, owner) \
                     ON k.monitor_id = s.monitor_id AND k.owner = s.owner",
                &[&ids, &owners],
            )
            .await?;
        rows.iter()
            .map(|r| {
                let observed = r
                    .get::<_, Option<serde_json::Value>>("observed")
                    .map(serde_json::from_value)
                    .transpose()?;
                Ok((
                    monitor_key(r),
                    SourceState {
                        observed,
                        polled_at: r.get("polled_at"),
                        next_due_at: r.get("next_due_at"),
                        error: r.get("error"),
                        failures: r.get::<_, i32>("failures").max(0) as u32,
                    },
                ))
            })
            .collect()
    }

    /// Take the poll slot for `key`: `true` when no poll is in flight and
    /// none is due later than now, in which case the next is now due at
    /// `next_due_at` and the caller polls. A monitor never polled gets a row.
    pub async fn take_poll_slot(
        &self,
        key: &MonitorKey,
        next_due_at: DateTime<Utc>,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        let taken = conn
            .query_opt(
                "INSERT INTO monitor_source_state (monitor_id, owner, next_due_at) \
                 VALUES ($1, $2, $3) \
                 ON CONFLICT (monitor_id, owner) DO UPDATE \
                   SET next_due_at = EXCLUDED.next_due_at, updated_at = now() \
                   WHERE monitor_source_state.next_due_at <= now() \
                 RETURNING 1::int AS ok",
                &[
                    &key.monitor_id,
                    &owner_col(key.owner.as_deref()),
                    &next_due_at,
                ],
            )
            .await?;
        Ok(taken.is_some())
    }

    /// Record a successful poll: what it saw, when the next is due, and a
    /// clean failure count.
    pub async fn record_poll(
        &self,
        key: &MonitorKey,
        observed: &ObservedState,
        next_due_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO monitor_source_state \
             (monitor_id, owner, observed, polled_at, next_due_at, error, failures) \
             VALUES ($1, $2, $3, now(), $4, NULL, 0) \
             ON CONFLICT (monitor_id, owner) DO UPDATE \
               SET observed = EXCLUDED.observed, polled_at = now(), \
                   next_due_at = EXCLUDED.next_due_at, error = NULL, failures = 0, \
                   updated_at = now()",
            &[
                &key.monitor_id,
                &owner_col(key.owner.as_deref()),
                &serde_json::to_value(observed)?,
                &next_due_at,
            ],
        )
        .await?;
        Ok(())
    }

    /// Record a failed poll, keeping what the last successful one saw, and
    /// return the consecutive-failure count it brings the monitor to.
    pub async fn record_poll_failure(
        &self,
        key: &MonitorKey,
        error: &str,
        next_due_at: DateTime<Utc>,
    ) -> Result<u32> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "INSERT INTO monitor_source_state \
                 (monitor_id, owner, polled_at, next_due_at, error, failures) \
                 VALUES ($1, $2, now(), $3, $4, 1) \
                 ON CONFLICT (monitor_id, owner) DO UPDATE \
                   SET polled_at = now(), next_due_at = EXCLUDED.next_due_at, \
                       error = EXCLUDED.error, \
                       failures = monitor_source_state.failures + 1, updated_at = now() \
                 RETURNING failures",
                &[
                    &key.monitor_id,
                    &owner_col(key.owner.as_deref()),
                    &next_due_at,
                    &error,
                ],
            )
            .await?;
        Ok(row.get::<_, i32>("failures").max(0) as u32)
    }

    /// Drop a monitor's observation state: it goes away, or starts afresh.
    pub async fn forget_source_state(&self, key: &MonitorKey) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM monitor_source_state WHERE monitor_id = $1 AND owner = $2",
            &[&key.monitor_id, &owner_col(key.owner.as_deref())],
        )
        .await?;
        Ok(())
    }

    /// Delete completed dispatch rows older than `retention`. Incomplete rows
    /// survive regardless of age so pending work is never dropped.
    pub async fn prune(&self, retention: Duration) -> Result<u64> {
        let conn = self.pool.get().await?;
        let secs = retention.as_secs() as i64;
        let n = conn
            .execute(
                "DELETE FROM monitor_dispatches \
                 WHERE completed_at IS NOT NULL \
                   AND completed_at < now() - ($1::bigint * interval '1 second')",
                &[&secs],
            )
            .await?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(MonitorStore) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            MonitorStore::build_pool,
            |pool| async move {
                let store = MonitorStore::new(pool).await.expect("store");
                test(store).await;
            },
        )
        .await;
    }

    fn ev(ts: &str) -> MonitorEvent {
        MonitorEvent::Message(workforce_common::channels::InboundEvent {
            surface: "slack".into(),
            workspace: "T1".into(),
            channel: "C1".into(),
            user: Some("U1".into()),
            bot_id: None,
            text: format!("message {ts}"),
            ts: ts.into(),
            thread_ts: None,
            has_files: false,
        })
    }

    fn observed(key: &str) -> MonitorEvent {
        MonitorEvent::Observed(crate::monitor::event::Observation {
            source: "jira.search".into(),
            key: key.into(),
            observed_at: Utc::now(),
            text: format!("issue {key}"),
            value: serde_json::json!({ "status": "done" }),
            previous: None,
            item: serde_json::json!({ "key": key }),
        })
    }

    fn key(message_id: &str) -> DispatchKey {
        DispatchKey {
            monitor_id: "mon1".into(),
            owner: Some("alice".into()),
            surface: "slack".into(),
            channel: "C1".into(),
            message_id: message_id.into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn claim_complete_and_dedup() {
        with_store(|s| async move {
            let k = key("100.001");
            let fresh = Duration::from_secs(3600);
            assert_eq!(
                s.claim(&k, &ev("100.001"), fresh).await.unwrap(),
                Claim::Won
            );
            // Second claim while fresh-in-flight is suppressed.
            assert_eq!(
                s.claim(&k, &ev("100.001"), fresh).await.unwrap(),
                Claim::InFlight
            );
            s.complete(&k, Some("inst-1")).await.unwrap();
            assert_eq!(
                s.claim(&k, &ev("100.001"), fresh).await.unwrap(),
                Claim::AlreadyDone
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_incomplete_is_reclaimed() {
        with_store(|s| async move {
            let k = key("200.002");
            assert_eq!(
                s.claim(&k, &ev("200.002"), Duration::from_secs(3600))
                    .await
                    .unwrap(),
                Claim::Won
            );
            // stale_after = 0 makes the existing incomplete claim immediately stale.
            assert_eq!(
                s.claim(&k, &ev("200.002"), Duration::from_secs(0))
                    .await
                    .unwrap(),
                Claim::Won
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recover_returns_incomplete_with_payload_only() {
        with_store(|s| async move {
            let done = key("300.003");
            let pending = key("300.004");
            s.claim(&done, &ev("300.003"), Duration::from_secs(3600))
                .await
                .unwrap();
            s.complete(&done, None).await.unwrap();
            s.claim(&pending, &ev("300.004"), Duration::from_secs(3600))
                .await
                .unwrap();

            let recovered = s.recover(Duration::from_secs(0), 10).await.unwrap();
            assert_eq!(recovered.len(), 1, "only the incomplete dispatch recovers");
            assert_eq!(recovered[0].key.message_id, "300.004");
            assert_eq!(recovered[0].event.text(), "message 300.004");
            assert_eq!(recovered[0].key.owner.as_deref(), Some("alice"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_observation_claims_recovers_and_lists_like_a_message() {
        with_store(|s| async move {
            let k = DispatchKey {
                monitor_id: "src1".into(),
                owner: Some("alice".into()),
                surface: "source".into(),
                channel: "jira.search".into(),
                message_id: "A-1@1700000000000".into(),
            };
            assert_eq!(
                s.claim(&k, &observed("A-1"), Duration::from_secs(3600))
                    .await
                    .unwrap(),
                Claim::Won
            );
            let recovered = s.recover(Duration::from_secs(0), 10).await.unwrap();
            assert_eq!(recovered.len(), 1);
            let obs = recovered[0].event.observation().expect("an observation");
            assert_eq!((obs.key.as_str(), obs.text.as_str()), ("A-1", "issue A-1"));
            assert_eq!(obs.item["key"], "A-1");
            s.complete(&k, None).await.unwrap();
            let rows = s.recent_activity("src1", Some("alice"), 10).await.unwrap();
            assert_eq!(rows[0].text, "issue A-1");
            assert_eq!(rows[0].channel, "jira.search");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_state_records_polls_counts_failures_durably_and_is_forgotten() {
        with_store(|s| async move {
            let key = mkey("src1", Some("alice"));
            let other = mkey("src2", None);
            assert!(s
                .source_states(std::slice::from_ref(&key))
                .await
                .unwrap()
                .is_empty());

            let due = Utc::now() + chrono::Duration::seconds(300);
            let observed = ObservedState {
                entries: std::collections::BTreeMap::from([(
                    "A-1".to_string(),
                    crate::monitor::observe::Entry {
                        hash: "h".into(),
                        value: Some(serde_json::json!("open")),
                        held: false,
                    },
                )]),
            };
            s.record_poll(&key, &observed, due).await.unwrap();
            let states = s
                .source_states(&[key.clone(), other.clone()])
                .await
                .unwrap();
            let state = &states[&key];
            assert_eq!(state.observed.as_ref(), Some(&observed));
            assert!(state.polled_at.is_some());
            assert_eq!(state.failures, 0);
            assert!(state.error.is_none());
            assert!(!state.due_at(Utc::now()));
            assert!(state.due_at(due));
            assert!(!states.contains_key(&other));

            assert_eq!(s.record_poll_failure(&key, "boom", due).await.unwrap(), 1);
            assert_eq!(
                s.record_poll_failure(&key, "boom again", due)
                    .await
                    .unwrap(),
                2
            );
            let state = &s.source_states(std::slice::from_ref(&key)).await.unwrap()[&key];
            assert_eq!(state.error.as_deref(), Some("boom again"));
            assert_eq!(
                state.observed.as_ref(),
                Some(&observed),
                "a failure keeps the last reading"
            );
            assert_eq!(PollStatus::from(state).failures, 2);

            assert_eq!(
                s.record_poll_failure(&other, "first", due).await.unwrap(),
                1
            );
            let fresh = &s.source_states(std::slice::from_ref(&other)).await.unwrap()[&other];
            assert_eq!(fresh.observed, None, "a failed first poll has seen nothing");

            s.record_poll(&key, &ObservedState::default(), due)
                .await
                .unwrap();
            assert_eq!(
                s.source_states(std::slice::from_ref(&key)).await.unwrap()[&key].failures,
                0
            );

            s.forget_source_state(&key).await.unwrap();
            assert!(s.source_states(&[key]).await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_poll_slot_is_taken_once_until_it_is_due_again() {
        with_store(|s| async move {
            let key = mkey("src1", Some("alice"));
            let later =
                workforce_postgres::test_support::now_micros() + chrono::Duration::seconds(60);
            assert!(
                s.take_poll_slot(&key, later).await.unwrap(),
                "a never-polled source is due"
            );
            assert!(
                !s.take_poll_slot(&key, later).await.unwrap(),
                "and then not until later"
            );
            let state = &s.source_states(std::slice::from_ref(&key)).await.unwrap()[&key];
            assert_eq!(state.observed, None);
            assert_eq!(state.next_due_at, later);

            s.record_poll(&key, &ObservedState::default(), Utc::now())
                .await
                .unwrap();
            assert!(
                s.take_poll_slot(&key, later).await.unwrap(),
                "due now is taken"
            );
            s.forget_source_state(&key).await.unwrap();
            assert!(
                s.take_poll_slot(&key, later).await.unwrap(),
                "forgotten is due again"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_payload_is_closed_instead_of_stalling_recovery() {
        with_store(|s| async move {
            let good = key("700.001");
            s.claim(&good, &ev("700.001"), Duration::from_secs(3600))
                .await
                .unwrap();
            let conn = s.pool.get().await.unwrap();
            conn.execute(
                "INSERT INTO monitor_dispatches \
                 (monitor_id, owner, surface, channel, message_id, payload) \
                 VALUES ('mon1', 'alice', 'source', 'jira.search', 'bad', '{\"neither\": true}'::jsonb)",
                &[],
            )
            .await
            .unwrap();
            let recovered = s.recover(Duration::from_secs(0), 10).await.unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].key.message_id, "700.001");
            let rows = s.recent_activity("mon1", Some("alice"), 10).await.unwrap();
            let bad = rows.iter().find(|r| r.message_id == "bad").unwrap();
            assert!(bad.completed_at.is_some());
            assert!(bad.error.as_deref().unwrap().starts_with("unreadable payload"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fail_transient_keeps_open_terminal_closes() {
        with_store(|s| async move {
            let transient = key("400.001");
            s.claim(&transient, &ev("400.001"), Duration::from_secs(3600))
                .await
                .unwrap();
            s.fail(&transient, "5xx", false).await.unwrap();
            assert_eq!(
                s.recover(Duration::from_secs(0), 10).await.unwrap().len(),
                1
            );

            let terminal = key("400.002");
            s.claim(&terminal, &ev("400.002"), Duration::from_secs(3600))
                .await
                .unwrap();
            s.fail(&terminal, "workflow gone", true).await.unwrap();
            assert_eq!(
                s.claim(&terminal, &ev("400.002"), Duration::from_secs(3600))
                    .await
                    .unwrap(),
                Claim::AlreadyDone,
                "terminal failure is closed, not retried"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prune_drops_only_completed() {
        with_store(|s| async move {
            let done = key("500.001");
            let pending = key("500.002");
            s.claim(&done, &ev("500.001"), Duration::from_secs(3600))
                .await
                .unwrap();
            s.complete(&done, None).await.unwrap();
            s.claim(&pending, &ev("500.002"), Duration::from_secs(3600))
                .await
                .unwrap();

            assert_eq!(s.prune(Duration::from_secs(0)).await.unwrap(), 1);
            // The incomplete dispatch survived pruning.
            assert_eq!(
                s.recover(Duration::from_secs(0), 10).await.unwrap().len(),
                1
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recent_activity_reports_outcome() {
        with_store(|s| async move {
            let k = key("600.001");
            s.claim(&k, &ev("600.001"), Duration::from_secs(3600))
                .await
                .unwrap();
            s.complete(&k, Some("inst-9")).await.unwrap();
            let rows = s.recent_activity("mon1", Some("alice"), 10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].text, "message 600.001");
            assert_eq!(rows[0].instance_id.as_deref(), Some("inst-9"));
            assert!(rows[0].completed_at.is_some());
        })
        .await;
    }

    fn mkey(id: &str, owner: Option<&str>) -> MonitorKey {
        MonitorKey {
            monitor_id: id.into(),
            owner: owner.map(str::to_string),
        }
    }

    fn bindings(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(surface, channel)| (surface.to_string(), channel.to_string()))
            .collect()
    }

    async fn cursor_of(
        store: &MonitorStore,
        key: &MonitorKey,
        surface: &str,
        channel: &str,
    ) -> Option<String> {
        store
            .channel_cursors(surface, channel)
            .await
            .unwrap()
            .get(key)
            .cloned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cursors_are_per_monitor_seeded_once_and_reset_only_on_resume() {
        with_store(|s| async move {
            let releases = mkey("ops-releases", Some("alice"));
            let digest = mkey("ops-digest", Some("bob"));
            let ops = bindings(&[("slack", "C0OPS")]);
            s.set_cursors(&releases, &ops, "100.0", false)
                .await
                .unwrap();
            s.set_cursors(&digest, &ops, "100.0", false).await.unwrap();

            // A second seed leaves an ongoing monitor where it is.
            s.set_cursors(&releases, &ops, "900.0", false)
                .await
                .unwrap();
            assert_eq!(
                cursor_of(&s, &releases, "slack", "C0OPS").await.as_deref(),
                Some("100.0")
            );

            // A resume overwrites — but only the monitor being resumed.
            s.set_cursors(&releases, &ops, "900.0", true).await.unwrap();
            assert_eq!(
                cursor_of(&s, &releases, "slack", "C0OPS").await.as_deref(),
                Some("900.0")
            );
            assert_eq!(
                cursor_of(&s, &digest, "slack", "C0OPS").await.as_deref(),
                Some("100.0"),
                "one monitor's resume never moves another's place"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn advancing_is_monotonic_and_creates_a_missing_row() {
        with_store(|s| async move {
            let key = mkey("ops-releases", Some("alice"));
            let keys = std::slice::from_ref(&key);
            // First sight: the advance creates the row.
            s.advance_cursors(keys, "slack", "C0OPS", "150.0")
                .await
                .unwrap();
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("150.0")
            );
            // A lower ts never moves it backward.
            s.advance_cursors(keys, "slack", "C0OPS", "120.0")
                .await
                .unwrap();
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("150.0")
            );
            s.advance_cursors(keys, "slack", "C0OPS", "200.0")
                .await
                .unwrap();
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("200.0")
            );
            // A sweep with no live monitors touches nothing.
            s.advance_cursors(&[], "slack", "C0OPS", "999.0")
                .await
                .unwrap();
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("200.0")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_cursors_drops_a_binding_the_monitor_no_longer_has() {
        with_store(|s| async move {
            let key = mkey("ops-releases", Some("alice"));
            s.set_cursors(
                &key,
                &bindings(&[("slack", "C0OPS"), ("teams", "C0OPS")]),
                "100.0",
                false,
            )
            .await
            .unwrap();
            assert!(cursor_of(&s, &key, "teams", "C0OPS").await.is_some());

            s.set_cursors(&key, &bindings(&[("slack", "C0OPS")]), "900.0", false)
                .await
                .unwrap();
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("100.0"),
                "the kept binding holds its place"
            );
            assert!(
                cursor_of(&s, &key, "teams", "C0OPS").await.is_none(),
                "the dropped binding is forgotten"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forget_cursors_clears_a_deleted_monitor() {
        with_store(|s| async move {
            let alice = mkey("ops-releases", Some("alice"));
            let system = mkey("ops-releases", None);
            let ops = bindings(&[("slack", "C0OPS")]);
            s.set_cursors(&alice, &ops, "100.0", false).await.unwrap();
            s.set_cursors(&system, &ops, "200.0", false).await.unwrap();

            s.forget_cursors(&alice).await.unwrap();
            assert!(cursor_of(&s, &alice, "slack", "C0OPS").await.is_none());
            assert_eq!(
                cursor_of(&s, &system, "slack", "C0OPS").await.as_deref(),
                Some("200.0"),
                "the same id under another namespace is a distinct monitor"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn channel_cursors_reads_only_that_channel_and_keys_by_owner() {
        with_store(|s| async move {
            let alice = mkey("ops-releases", Some("alice"));
            let system = mkey("ops-releases", None);
            s.set_cursors(
                &alice,
                &bindings(&[("slack", "C0OPS"), ("slack", "C0DEV")]),
                "100.0",
                false,
            )
            .await
            .unwrap();
            s.set_cursors(&system, &bindings(&[("slack", "C0OPS")]), "200.0", false)
                .await
                .unwrap();

            let ops = s.channel_cursors("slack", "C0OPS").await.unwrap();
            assert_eq!(ops.len(), 2);
            assert_eq!(ops.get(&alice).map(String::as_str), Some("100.0"));
            assert_eq!(ops.get(&system).map(String::as_str), Some("200.0"));

            let dev = s.channel_cursors("slack", "C0DEV").await.unwrap();
            assert_eq!(dev.keys().collect::<Vec<_>>(), vec![&alice]);
        })
        .await;
    }

    const OPS_RELEASES_TOML: &str = r#"
[monitor]
id = "ops-releases"
name = "Release forwarder"
[monitor.binding]
surface = "slack"
channel = "C0OPS"
[monitor.filter]
type = "all"
[monitor.action]
type = "notify"
target = "C0OUT"
template = "x"
"#;

    /// The V018 backfill, exercised by re-running the file against the
    /// pre-upgrade state it was written for. Legitimate only because the file
    /// is idempotent (`IF NOT EXISTS` + `ON CONFLICT DO NOTHING`), and a raw
    /// `batch_execute` does not touch refinery's history.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cursor_backfill_adopts_the_channel_position_and_re_runs_as_a_no_op() {
        const BACKFILL: &str =
            include_str!("../../../common/migrations/telemetry/V018__monitor_cursors.sql");
        with_store(|s| async move {
            let defs = crate::workflow_store::WorkflowStore::new(s.pool.clone())
                .await
                .expect("definition store");
            let def =
                crate::monitor::MonitorDefinition::from_toml(OPS_RELEASES_TOML).expect("monitor");
            defs.save(
                "ops-releases",
                crate::workflow_store::KIND_MONITOR,
                OPS_RELEASES_TOML,
                Some(serde_json::to_value(&def).unwrap()),
                Some("alice"),
                workforce_core::access::Visibility::Private,
                crate::workflow_store::RunAsWrite::Unchanged,
                crate::workflow_store::EditContext::new("test", Some("alice")),
            )
            .await
            .unwrap();

            let conn = s.pool.get().await.unwrap();
            conn.execute(
                "INSERT INTO monitor_channel_cursors (surface, channel, last_swept_ts) \
                 VALUES ($1, $2, $3)",
                &[&"slack", &"C0OPS", &"100.0"],
            )
            .await
            .unwrap();
            assert!(
                s.channel_cursors("slack", "C0OPS")
                    .await
                    .unwrap()
                    .is_empty(),
                "the pre-upgrade state has no per-monitor cursor"
            );

            conn.batch_execute(BACKFILL).await.unwrap();
            let key = mkey("ops-releases", Some("alice"));
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("100.0"),
                "the monitor adopts its channel's position"
            );

            // Re-running never resets a cursor the new code has since moved.
            s.advance_cursors(std::slice::from_ref(&key), "slack", "C0OPS", "300.0")
                .await
                .unwrap();
            conn.batch_execute(BACKFILL).await.unwrap();
            assert_eq!(
                cursor_of(&s, &key, "slack", "C0OPS").await.as_deref(),
                Some("300.0")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn block_is_fresh_once_per_channel_binding_and_unblocks_by_channel() {
        with_store(|s| async move {
            let alice = mkey("m1", Some("alice"));
            let system = mkey("m1", None);
            assert!(s
                .block(&alice, "atlas", "slack", "C1", "not_in_channel")
                .await
                .unwrap());
            assert!(
                !s.block(&alice, "atlas", "slack", "C1", "still")
                    .await
                    .unwrap(),
                "a repeat sighting is not fresh"
            );
            let blocks = s.blocks().await.unwrap();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].key, alice);
            assert_eq!(blocks[0].error, "still", "latest error is kept");

            // Same id under the system namespace is a distinct monitor.
            assert!(s.block(&system, "atlas", "slack", "C1", "x").await.unwrap());
            assert!(s.blocks().await.unwrap().iter().any(|b| b.key == system));

            // Rebound to another channel → fresh again.
            assert!(s.block(&alice, "atlas", "slack", "C2", "x").await.unwrap());
            assert_eq!(
                s.blocks()
                    .await
                    .unwrap()
                    .iter()
                    .find(|b| b.key == alice)
                    .unwrap()
                    .channel,
                "C2"
            );

            // Unblocking C1 releases only the monitors still bound to it.
            let released = s.unblock_channel("atlas", "slack", "C1").await.unwrap();
            assert_eq!(released, vec![system.clone()]);
            assert!(s
                .unblock_channel("atlas", "slack", "C1")
                .await
                .unwrap()
                .is_empty());

            // Another agent's block on the same channel is untouched.
            assert!(s.block(&system, "candy", "slack", "C2", "x").await.unwrap());
            let released = s.unblock_channel("atlas", "slack", "C2").await.unwrap();
            assert_eq!(released, vec![alice.clone()]);
            assert_eq!(s.blocks().await.unwrap().len(), 1);

            s.forget_block(&system).await.unwrap();
            assert!(s.blocks().await.unwrap().is_empty());
        })
        .await;
    }
}
