//! A boss's update state in Postgres (`boss_update_state`): what it is
//! gathering, when it last told the person, and what each session's
//! account said then; an update written is kept before it goes out, so a
//! restart neither loses it nor posts it twice.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::Result;
use uuid::Uuid;

use crate::store::DeskStore;
use crate::update::{Pending, Told, UpdateState};

impl DeskStore {
    /// `boss`'s update state, begun at `now` when it has none.
    pub async fn update_state(&self, boss: &str, now: DateTime<Utc>) -> Result<UpdateState> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO boss_update_state (boss, since) VALUES ($1, $2) \
                 ON CONFLICT (boss) DO NOTHING",
                &[&boss, &now],
            )
            .await?;
        let r = client
            .query_one(
                "SELECT boss, since, gather_since, result, last_update_at, unread, told, pending, \
                   away_since \
                 FROM boss_update_state WHERE boss = $1",
                &[&boss],
            )
            .await?;
        let told: HashMap<Uuid, Told> = serde_json::from_value(r.try_get(6)?).unwrap_or_default();
        let pending: Option<serde_json::Value> = r.try_get(7)?;
        Ok(UpdateState {
            boss: r.try_get(0)?,
            since: r.try_get(1)?,
            gather_since: r.try_get(2)?,
            result: r.try_get(3)?,
            last_update_at: r.try_get(4)?,
            unread: u32::try_from(r.try_get::<_, i32>(5)?).unwrap_or(0),
            told,
            pending: pending.and_then(|p| match serde_json::from_value(p) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "a pending update couldn't be read; gathered again");
                    None
                }
            }),
            away_since: r.try_get(8)?,
        })
    }

    /// Starts gathering `boss`'s update at `at` unless it already is; a
    /// `result` in it makes it go sooner.
    pub async fn update_gather(&self, boss: &str, at: DateTime<Utc>, result: bool) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_update_state SET gather_since = COALESCE(gather_since, $2), \
                   result = result OR $3 WHERE boss = $1",
                &[&boss, &at, &result],
            )
            .await?;
        Ok(())
    }

    /// Keeps `p` as `boss`'s update about to go out.
    pub async fn update_pend(&self, boss: &str, p: &Pending) -> Result<()> {
        let client = self.pool().get().await?;
        let v = serde_json::to_value(p)?;
        client
            .execute(
                "UPDATE boss_update_state SET pending_id = $2, pending = $3 WHERE boss = $1",
                &[&boss, &p.id, &v],
            )
            .await?;
        Ok(())
    }

    /// `boss` told the person at `at`, with `told` what each session's
    /// account said then and `unread` its updates unread in a row: the
    /// gather starts again and nothing is pending.
    pub async fn update_sent(
        &self,
        boss: &str,
        at: DateTime<Utc>,
        told: &HashMap<Uuid, Told>,
        unread: u32,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        let told = serde_json::to_value(told)?;
        let unread = i32::try_from(unread).unwrap_or(i32::MAX);
        client
            .execute(
                "UPDATE boss_update_state SET last_update_at = $2, told = $3, unread = $4, \
                   gather_since = NULL, result = false, pending_id = NULL, pending = NULL \
                 WHERE boss = $1",
                &[&boss, &at, &told, &unread],
            )
            .await?;
        Ok(())
    }

    /// Drops `boss`'s pending update, which didn't go: it is gathered
    /// again.
    pub async fn update_unpend(&self, boss: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_update_state SET pending_id = NULL, pending = NULL WHERE boss = $1",
                &[&boss],
            )
            .await?;
        Ok(())
    }

    /// Stops gathering `boss`'s update: what it gathered says nothing now.
    /// When it last told the person stays as it was.
    pub async fn update_regather(&self, boss: &str, told: &HashMap<Uuid, Told>) -> Result<()> {
        let client = self.pool().get().await?;
        let told = serde_json::to_value(told)?;
        client
            .execute(
                "UPDATE boss_update_state SET gather_since = NULL, result = false, told = $2 \
                 WHERE boss = $1",
                &[&boss, &told],
            )
            .await?;
        Ok(())
    }

    /// The person is away from `boss`'s DM since `since`, unless they
    /// already were: the first time it is noticed is kept.
    pub async fn update_away(&self, boss: &str, since: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_update_state SET away_since = COALESCE(away_since, $2) \
                 WHERE boss = $1",
                &[&boss, &since],
            )
            .await?;
        Ok(())
    }

    /// The person is back from the absence that began at `since`: whether
    /// this call ended it (a second caller, or one after a restart, finds
    /// it ended).
    pub async fn update_back(&self, boss: &str, since: DateTime<Utc>) -> Result<bool> {
        let client = self.pool().get().await?;
        let n = client
            .execute(
                "UPDATE boss_update_state SET away_since = NULL \
                 WHERE boss = $1 AND away_since = $2",
                &[&boss, &since],
            )
            .await?;
        Ok(n == 1)
    }
}
