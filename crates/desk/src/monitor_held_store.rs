//! What the person's monitors saw, held for them by their cadence until it
//! may be told (`monitor_held`): through the quiet hours, until the
//! roundup, or while they are away from the DM it is for.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::store::DeskStore;

/// Why a monitor's finding waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeldFor {
    /// The quiet hours: until the first roundup after them.
    Quiet,
    /// The person's cadence puts it in the roundup.
    Roundup,
    /// The person is away from the DM: until they are back.
    Away,
    /// The person is typing in the DM, or the agent's reply there is being
    /// written: until that is done.
    Typing,
}

impl HeldFor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Roundup => "roundup",
            Self::Away => "away",
            Self::Typing => "typing",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "quiet" => Self::Quiet,
            "roundup" => Self::Roundup,
            "away" => Self::Away,
            _ => Self::Typing,
        }
    }
}

/// One finding held for the person.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldFinding {
    pub id: Uuid,
    /// The agent whose DM it is told in.
    pub agent: String,
    /// The person it is for.
    pub owner: String,
    pub monitor_id: String,
    pub monitor_name: String,
    /// Told as a Desk notice, not a message for them.
    pub notice: bool,
    pub text: String,
    pub reason: HeldFor,
    pub held_at: DateTime<Utc>,
    /// Told no sooner than this, and only once the person is back.
    pub release_at: DateTime<Utc>,
}

const COLUMNS: &str =
    "id, agent, owner, monitor_id, monitor_name, place, text, reason, held_at, release_at";

fn held_of(r: &Row) -> Result<HeldFinding> {
    let place: String = r.try_get(5)?;
    let reason: String = r.try_get(7)?;
    Ok(HeldFinding {
        id: r.try_get(0)?,
        agent: r.try_get(1)?,
        owner: r.try_get(2)?,
        monitor_id: r.try_get(3)?,
        monitor_name: r.try_get(4)?,
        notice: place == "notice",
        text: r.try_get(6)?,
        reason: HeldFor::parse(&reason),
        held_at: r.try_get(8)?,
        release_at: r.try_get(9)?,
    })
}

impl DeskStore {
    /// Keeps `held` until it may be told.
    pub async fn monitor_hold(&self, held: &HeldFinding) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO monitor_held ({COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (id) DO NOTHING"
                ),
                &[
                    &held.id,
                    &held.agent,
                    &held.owner,
                    &held.monitor_id,
                    &held.monitor_name,
                    &(if held.notice { "notice" } else { "dm" }),
                    &held.text,
                    &held.reason.as_str(),
                    &held.held_at,
                    &held.release_at,
                ],
            )
            .await
            .context("holding what a monitor saw")?;
        Ok(())
    }

    /// Everything held whose time has come by `now` and that no one is
    /// telling, oldest first.
    pub async fn monitor_held_due(&self, now: DateTime<Utc>) -> Result<Vec<HeldFinding>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM monitor_held WHERE release_at <= $1 \
                     AND (claimed_until IS NULL OR claimed_until < now()) \
                     ORDER BY held_at LIMIT 500"
                ),
                &[&now],
            )
            .await
            .context("reading what monitors saw")?;
        rows.iter().map(held_of).collect()
    }

    /// Everything held for `agent`'s DM, due or not, oldest first.
    pub async fn monitor_held_for(&self, agent: &str) -> Result<Vec<HeldFinding>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM monitor_held WHERE agent = $1 ORDER BY held_at LIMIT 500"
                ),
                &[&agent],
            )
            .await?;
        rows.iter().map(held_of).collect()
    }

    /// Takes `ids` to tell them, for a minute: the ones this call took
    /// (another caller holds the rest, or they went). A telling that fails
    /// or dies with the hub leaves them to be taken again once the minute
    /// is past.
    pub async fn monitor_held_take(&self, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "UPDATE monitor_held SET claimed_until = now() + interval '1 minute' \
                 WHERE id = ANY($1) AND (claimed_until IS NULL OR claimed_until < now()) \
                 RETURNING id",
                &[&ids],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// `ids` were told: forgotten.
    pub async fn monitor_held_done(&self, ids: &[Uuid]) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute("DELETE FROM monitor_held WHERE id = ANY($1)", &[&ids])
            .await?;
        Ok(())
    }

    /// Lets `ids`, taken but not told, be taken again at once.
    pub async fn monitor_held_untake(&self, ids: &[Uuid]) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE monitor_held SET claimed_until = NULL WHERE id = ANY($1)",
                &[&ids],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SubsecRound;
    use workforce_postgres::test_support::with_test_pool;

    fn held(agent: &str, release_at: DateTime<Utc>, text: &str) -> HeldFinding {
        HeldFinding {
            id: Uuid::new_v4(),
            agent: agent.into(),
            owner: "person-1".into(),
            monitor_id: "invoices".into(),
            monitor_name: "Invoices".into(),
            notice: false,
            text: text.into(),
            reason: HeldFor::Quiet,
            // Postgres keeps microseconds.
            held_at: Utc::now().trunc_subsecs(6),
            release_at: release_at.trunc_subsecs(6),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn held_findings_wait_for_their_time_are_taken_once_and_forgotten_when_told() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let now = Utc::now();
            let due = held(
                "scout",
                now - chrono::Duration::minutes(1),
                "Invoice from Acme",
            );
            let later = held(
                "scout",
                now + chrono::Duration::hours(8),
                "Invoice from Globex",
            );
            store.monitor_hold(&due).await.unwrap();
            store.monitor_hold(&later).await.unwrap();
            // Held again (a retried dispatch): still one.
            store.monitor_hold(&due).await.unwrap();

            assert_eq!(
                store.monitor_held_due(now).await.unwrap(),
                vec![due.clone()]
            );
            assert_eq!(store.monitor_held_for("scout").await.unwrap().len(), 2);

            assert_eq!(
                store.monitor_held_take(&[due.id]).await.unwrap(),
                vec![due.id]
            );
            assert!(
                store.monitor_held_take(&[due.id]).await.unwrap().is_empty(),
                "taken once"
            );
            assert!(store.monitor_held_due(now).await.unwrap().is_empty());
            store.monitor_held_untake(&[due.id]).await.unwrap();
            assert_eq!(store.monitor_held_due(now).await.unwrap().len(), 1);

            store.monitor_held_done(&[due.id]).await.unwrap();
            assert!(store.monitor_held_due(now).await.unwrap().is_empty());
            assert_eq!(store.monitor_held_for("scout").await.unwrap(), vec![later]);
        })
        .await;
    }
}
