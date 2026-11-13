//! The boss's decisions in Postgres (`boss_decisions`): kept as they are
//! made, listed per boss, and undone once: the claim on an undo is one
//! statement, so two presses never undo twice.

use chrono::{DateTime, Utc};
use eyre::Result;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::decision::{BossDecision, Handled, How, Need, Undo};
use crate::store::DeskStore;

const COLUMNS: &str =
    "id, boss, owner, at, session, need, need_ref, what, why, how, preference, action, undo, \
     undone_at, event";

fn decision(r: &Row) -> Result<BossDecision> {
    Ok(BossDecision {
        id: r.try_get(0)?,
        boss: r.try_get(1)?,
        owner: r.try_get(2)?,
        at: r.try_get(3)?,
        session: r.try_get(4)?,
        need: Need::parse(r.try_get(5)?)?,
        need_ref: r.try_get(6)?,
        what: r.try_get(7)?,
        why: r.try_get(8)?,
        how: How::parse(r.try_get(9)?)?,
        preference: r.try_get(10)?,
        action: r.try_get(11)?,
        undo: Undo::parse(r.try_get(12)?)?,
        undone_at: r.try_get(13)?,
        event: r.try_get(14)?,
    })
}

impl DeskStore {
    /// Keeps `d`, new.
    pub async fn decision_put(&self, d: &BossDecision) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO boss_decisions ({COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"
                ),
                &[
                    &d.id,
                    &d.boss,
                    &d.owner,
                    &d.at,
                    &d.session,
                    &d.need.as_str(),
                    &d.need_ref,
                    &d.what,
                    &d.why,
                    &d.how.as_str(),
                    &d.preference,
                    &d.action,
                    &d.undo.as_str(),
                    &d.undone_at,
                    &d.event,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn decision(&self, id: Uuid) -> Result<Option<BossDecision>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM boss_decisions WHERE id = $1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(decision)
            .transpose()
    }

    /// `boss`'s decisions since `since`, oldest first, at most `limit` of
    /// the newest.
    pub async fn decisions_since(
        &self,
        boss: &str,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<BossDecision>> {
        let client = self.pool().get().await?;
        let mut out = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM boss_decisions WHERE boss = $1 AND at >= $2 \
                     ORDER BY at DESC, id LIMIT $3"
                ),
                &[&boss, &since, &limit],
            )
            .await?
            .iter()
            .map(decision)
            .collect::<Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }

    /// How many needs `boss` handled since `since`, over every decision in
    /// that time (never a page of them).
    pub async fn decisions_handled(&self, boss: &str, since: DateTime<Utc>) -> Result<Handled> {
        let client = self.pool().get().await?;
        let mut h = Handled::default();
        for r in client
            .query(
                "SELECT need, how, count(*) AS n FROM boss_decisions \
                 WHERE boss = $1 AND at >= $2 GROUP BY need, how",
                &[&boss, &since],
            )
            .await?
        {
            let n: i64 = r.try_get("n")?;
            if let (Ok(need), Ok(how)) = (
                Need::parse(r.try_get("need")?),
                How::parse(r.try_get("how")?),
            ) {
                h.add(need, how, n as usize);
            }
        }
        Ok(h)
    }

    /// [`Self::decisions_handled`] per day at the person's `tz`, oldest
    /// first: only the days with a need.
    pub async fn decisions_handled_by_day(
        &self,
        boss: &str,
        since: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Result<Vec<(chrono::NaiveDate, Handled)>> {
        let client = self.pool().get().await?;
        let mut days: Vec<(chrono::NaiveDate, Handled)> = Vec::new();
        for r in client
            .query(
                "SELECT (at AT TIME ZONE $3)::date AS day, need, how, count(*) AS n \
                 FROM boss_decisions WHERE boss = $1 AND at >= $2 \
                 GROUP BY day, need, how ORDER BY day",
                &[&boss, &since, &tz.name()],
            )
            .await?
        {
            let day: chrono::NaiveDate = r.try_get("day")?;
            let n: i64 = r.try_get("n")?;
            let (Ok(need), Ok(how)) = (
                Need::parse(r.try_get("need")?),
                How::parse(r.try_get("how")?),
            ) else {
                continue;
            };
            if days.last().is_none_or(|(d, _)| *d != day) {
                days.push((day, Handled::default()));
            }
            if let Some((_, h)) = days.last_mut() {
                h.add(need, how, n as usize);
            }
        }
        days.retain(|(_, h)| h.needs() > 0);
        Ok(days)
    }

    /// Whether `boss` already gave the person the need `need_ref` since
    /// `since`: a suggestion made again is one escalation, not two.
    pub async fn decision_escalated_since(
        &self,
        boss: &str,
        need_ref: &str,
        since: DateTime<Utc>,
    ) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt(
                "SELECT 1 FROM boss_decisions WHERE boss = $1 AND need_ref = $2 \
                 AND how = 'escalated' AND at >= $3 LIMIT 1",
                &[&boss, &need_ref, &since],
            )
            .await?
            .is_some())
    }

    /// The decisions `ids` (as the chat's extras name them), by id.
    pub async fn decisions_of(&self, ids: &[Uuid]) -> Result<Vec<BossDecision>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {COLUMNS} FROM boss_decisions WHERE id = ANY($1)"),
                &[&ids],
            )
            .await?
            .iter()
            .map(decision)
            .collect()
    }

    /// Claims `id`'s undo: the decision as it now is, marked undone, or
    /// `None` when it is already undone (or not there).
    pub async fn decision_claim_undo(&self, id: Uuid) -> Result<Option<BossDecision>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE boss_decisions SET undone_at = now() \
                     WHERE id = $1 AND undone_at IS NULL RETURNING {COLUMNS}"
                ),
                &[&id],
            )
            .await?
            .as_ref()
            .map(decision)
            .transpose()
    }

    /// Lets go of a claim on `id`'s undo that didn't take.
    pub async fn decision_unclaim_undo(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE boss_decisions SET undone_at = NULL WHERE id = $1",
                &[&id],
            )
            .await?;
        Ok(())
    }
}
