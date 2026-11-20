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
    /// The person is typing in the DM, the agent's reply there is being
    /// written, or older findings for it wait: until that is done.
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
    /// [`HeldFinding::id_of`] its dispatch: one row however often it is
    /// held.
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

impl HeldFinding {
    /// The row a finding is kept as, from what makes it one (its monitor,
    /// the person, the event it saw): a dispatch run again holds it again
    /// as the same row, not a second one.
    pub fn id_of(key: &str) -> Uuid {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("monitor_held\n{key}").as_bytes(),
        )
    }
}

/// The most of one DM's due findings read at a look: the rest come at the
/// next one.
const DUE_PER_DM: i64 = 200;

/// The failed tellings after which a finding is dropped: put off a minute
/// more after each, it has had about an hour.
pub const ATTEMPTS: i32 = 10;

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

    /// What is held whose time has come by `now`, oldest first: the oldest
    /// [`DUE_PER_DM`] of each DM and place, so one busy monitor keeps no
    /// other DM waiting, and none of a DM whose findings are being told, so
    /// what came since goes after them, not beside them.
    pub async fn monitor_held_due(&self, now: DateTime<Utc>) -> Result<Vec<HeldFinding>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM ( \
                       SELECT *, row_number() OVER ( \
                         PARTITION BY agent, owner, place ORDER BY held_at, id) AS nth \
                       FROM monitor_held m WHERE release_at <= $1 \
                       AND (claimed_until IS NULL OR claimed_until < now()) \
                       AND NOT EXISTS (SELECT 1 FROM monitor_held t WHERE t.agent = m.agent \
                         AND t.owner = m.owner AND t.place = m.place \
                         AND t.claimed_until >= now())) due \
                     WHERE nth <= $2 ORDER BY held_at, id"
                ),
                &[&now, &DUE_PER_DM],
            )
            .await
            .context("reading what monitors saw")?;
        rows.iter().map(held_of).collect()
    }

    /// Whether anything is held for `owner`'s DM with `agent` (as a notice
    /// or not) whose time has come by `now`: what comes now goes after it.
    pub async fn monitor_held_waiting(
        &self,
        agent: &str,
        owner: &str,
        notice: bool,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let client = self.pool().get().await?;
        let place = if notice { "notice" } else { "dm" };
        let row = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM monitor_held WHERE agent = $1 AND owner = $2 \
                 AND place = $3 AND release_at <= $4)",
                &[&agent, &owner, &place, &now],
            )
            .await?;
        Ok(row.try_get(0)?)
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

    /// `ids` were taken but their telling failed: put off a minute more for
    /// each failure so far (what comes since for their DM isn't held behind
    /// them meanwhile), and dropped past [`ATTEMPTS`]: the monitors of those
    /// dropped. Only rows still taken are let go.
    ///
    /// The minute is the wall clock's, whatever the crew's minute is.
    pub async fn monitor_held_untake(&self, ids: &[Uuid]) -> Result<Vec<String>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let dropped = tx
            .query(
                "DELETE FROM monitor_held WHERE id = ANY($1) AND claimed_until IS NOT NULL \
                 AND attempts + 1 >= $2 RETURNING monitor_id",
                &[&ids, &ATTEMPTS],
            )
            .await?;
        tx.execute(
            "UPDATE monitor_held SET claimed_until = NULL, attempts = attempts + 1, \
             release_at = now() + (attempts + 1) * interval '1 minute' \
             WHERE id = ANY($1) AND claimed_until IS NOT NULL",
            &[&ids],
        )
        .await?;
        tx.commit().await?;
        dropped.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SubsecRound;
    use workforce_postgres::test_support::with_test_pool;

    fn held(agent: &str, release_at: DateTime<Utc>, text: &str) -> HeldFinding {
        HeldFinding {
            id: HeldFinding::id_of(&format!("invoices\nperson-1\n{agent}\n{text}")),
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
            // Held again (a retried dispatch, a moment later): still one.
            let again = HeldFinding {
                held_at: due.held_at + chrono::Duration::seconds(5),
                ..due.clone()
            };
            store.monitor_hold(&again).await.unwrap();
            assert!(store
                .monitor_held_waiting("scout", "person-1", false, now)
                .await
                .unwrap());
            assert!(!store
                .monitor_held_waiting("scout", "person-1", true, now)
                .await
                .unwrap());
            assert!(!store
                .monitor_held_waiting("lynx", "person-1", false, now)
                .await
                .unwrap());

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
            // Come since, while the first is being told: after it.
            let since = held(
                "scout",
                now - chrono::Duration::seconds(1),
                "Invoice from Initech",
            );
            store.monitor_hold(&since).await.unwrap();
            assert!(store.monitor_held_due(now).await.unwrap().is_empty());
            store.monitor_held_done(&[since.id]).await.unwrap();
            // Its telling failed: put off a minute, holding nothing up meanwhile.
            assert!(store
                .monitor_held_untake(&[due.id])
                .await
                .unwrap()
                .is_empty());
            assert!(store.monitor_held_due(Utc::now()).await.unwrap().is_empty());
            assert!(!store
                .monitor_held_waiting("scout", "person-1", false, Utc::now())
                .await
                .unwrap());
            let later_on = Utc::now() + chrono::Duration::minutes(2);
            assert_eq!(store.monitor_held_due(later_on).await.unwrap().len(), 1);
            // Failing every time, it is dropped in the end.
            for _ in 1..ATTEMPTS - 1 {
                store.monitor_held_take(&[due.id]).await.unwrap();
                assert!(store
                    .monitor_held_untake(&[due.id])
                    .await
                    .unwrap()
                    .is_empty());
            }
            store.monitor_held_take(&[due.id]).await.unwrap();
            assert_eq!(
                store.monitor_held_untake(&[due.id]).await.unwrap(),
                vec!["invoices".to_string()],
                "dropped"
            );
            assert_eq!(
                store.monitor_held_for("scout").await.unwrap(),
                vec![later.clone()]
            );
            store.monitor_hold(&due).await.unwrap();
            assert_eq!(store.monitor_held_due(now).await.unwrap().len(), 1);

            store.monitor_held_done(&[due.id]).await.unwrap();
            assert!(store.monitor_held_due(now).await.unwrap().is_empty());
            assert_eq!(store.monitor_held_for("scout").await.unwrap(), vec![later]);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_busy_dm_keeps_no_other_waiting() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let now = Utc::now();
            let due = now - chrono::Duration::minutes(1);
            for n in 0..DUE_PER_DM + 5 {
                store
                    .monitor_hold(&held("scout", due, &format!("invoice {n}")))
                    .await
                    .unwrap();
            }
            let other = held("lynx", due, "Invoice from Initech");
            store.monitor_hold(&other).await.unwrap();
            let read = store.monitor_held_due(now).await.unwrap();
            assert_eq!(read.len() as i64, DUE_PER_DM + 1);
            assert!(read.contains(&other), "the quiet DM is read too");
        })
        .await;
    }
}
