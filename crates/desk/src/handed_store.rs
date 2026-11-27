//! Handed requests and the words owed back for them, in Postgres
//! ([`crate::handed`]).

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::handed::{Handed, Kept, Owed};
use crate::store::DeskStore;

/// How long a request is kept, and looked at for its plan's stalls.
pub const KEPT_DAYS: i32 = 30;
/// How many times a word owed is tried before it is given up on.
pub const SAY_TRIES: i32 = 50;

const COLUMNS: &str = "id, agent, agent_name, place, words, session, asked_at, boss, delivered_at";

fn handed_of(r: &Row) -> Result<Handed> {
    Ok(Handed {
        id: r.try_get(0)?,
        agent: r.try_get(1)?,
        agent_name: r.try_get(2)?,
        place: r.try_get(3)?,
        words: r.try_get(4)?,
        session: r.try_get(5)?,
        asked_at: r.try_get(6)?,
        boss: r.try_get(7)?,
        delivered_at: r.try_get(8)?,
    })
}

impl DeskStore {
    /// Keeps `h` as taken, unless it is kept already, and what became of it
    /// then.
    pub async fn handed_keep(&self, h: &Handed) -> Result<Kept> {
        let client = self.pool().get().await?;
        let made = client
            .execute(
                "INSERT INTO handed (agent, id, agent_name, place, words, session, asked_at, taken_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now()) ON CONFLICT (agent, id) DO NOTHING",
                &[&h.agent, &h.id, &h.agent_name, &h.place, &h.words, &h.session, &h.asked_at],
            )
            .await
            .context("keeping a handed request")?;
        if made == 1 {
            return Ok(Kept::New);
        }
        let row = client
            .query_one(
                "SELECT boss, delivered_at IS NOT NULL, \
                   EXISTS (SELECT 1 FROM handed_says s WHERE s.agent = h.agent AND s.handed = h.id) \
                 FROM handed h WHERE agent = $1 AND id = $2",
                &[&h.agent, &h.id],
            )
            .await
            .context("reading a handed request kept")?;
        let boss: Option<String> = row.try_get(0)?;
        Ok(
            match (boss, row.try_get::<_, bool>(1)?, row.try_get::<_, bool>(2)?) {
                (_, _, true) => Kept::Done,
                (Some(boss), true, false) => Kept::Unanswered { boss },
                _ => Kept::Undelivered,
            },
        )
    }

    /// `h` delivered to `boss`, its words in the boss's DM since `at`.
    pub async fn handed_delivered(&self, h: &Handed, boss: &str, at: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE handed SET boss = $3, delivered_at = $4 WHERE agent = $1 AND id = $2",
                &[&h.agent, &h.id, &boss, &at],
            )
            .await
            .context("keeping where a handed request went")?;
        Ok(())
    }

    /// The requests delivered to `boss` lately.
    pub async fn handed_of_boss(&self, boss: &str) -> Result<Vec<Handed>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM handed WHERE boss = $1 \
                     AND delivered_at > now() - make_interval(days => $2) ORDER BY delivered_at"
                ),
                &[&boss, &KEPT_DAYS],
            )
            .await
            .context("reading a boss's handed requests")?
            .iter()
            .map(handed_of)
            .collect()
    }

    /// Claims agent `agent`'s request `id` for the plan its words agree,
    /// unless one claimed it already: whether this did.
    pub async fn handed_use(&self, agent: &str, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        let claimed = client
            .query_opt(
                "UPDATE handed SET used_at = now() \
                 WHERE agent = $1 AND id = $2 AND used_at IS NULL RETURNING id",
                &[&agent, &id],
            )
            .await
            .context("claiming a handed request for a plan")?;
        Ok(claimed.is_some())
    }

    /// Lets go the claim on agent `agent`'s request `id`: its plan wasn't
    /// saved.
    pub async fn handed_unuse(&self, agent: &str, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE handed SET used_at = NULL WHERE agent = $1 AND id = $2",
                &[&agent, &id],
            )
            .await
            .context("letting go a handed request's claim")?;
        Ok(())
    }

    /// One word owed back for `h`.
    pub async fn handed_say(&self, h: &Handed, text: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO handed_says (agent, handed, text, at) VALUES ($1, $2, $3, now())",
                &[&h.agent, &h.id, &text],
            )
            .await
            .context("keeping a word owed for a handed request")?;
        Ok(())
    }

    /// The words owed and not yet sent, oldest first: at most `each` of
    /// each agent's, so one whose hub is down never holds up another's.
    /// One tried [`SAY_TRIES`] times, or older than [`KEPT_DAYS`], is given
    /// up on.
    pub async fn handed_owed(&self, each: i64) -> Result<Vec<Owed>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, handed, agent, text FROM ( \
                   SELECT s.id, s.handed, s.agent, s.text, s.at, \
                     row_number() OVER (PARTITION BY s.agent ORDER BY s.at, s.id) AS n \
                   FROM handed_says s WHERE s.sent_at IS NULL AND s.tries < $2 \
                     AND s.at > now() - make_interval(days => $3) \
                 ) owed WHERE n <= $1 ORDER BY at, id",
                &[&each, &SAY_TRIES, &KEPT_DAYS],
            )
            .await
            .context("reading the words owed for handed requests")?
            .iter()
            .map(|r| {
                Ok(Owed {
                    id: r.try_get(0)?,
                    handed: r.try_get(1)?,
                    agent: r.try_get(2)?,
                    text: r.try_get(3)?,
                })
            })
            .collect()
    }

    /// The owed word `id` sent, or never to be.
    pub async fn handed_sent(&self, id: i64) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE handed_says SET sent_at = now() WHERE id = $1",
                &[&id],
            )
            .await
            .context("marking a word owed as sent")?;
        Ok(())
    }

    /// The owed word `id` tried once more, and not taken.
    pub async fn handed_tried(&self, id: i64) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE handed_says SET tries = tries + 1 WHERE id = $1",
                &[&id],
            )
            .await
            .context("counting a word owed's try")?;
        Ok(())
    }

    /// Forgets the requests taken more than [`KEPT_DAYS`] ago, with their
    /// words: how many.
    pub async fn handed_prune(&self) -> Result<u64> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM handed WHERE taken_at < now() - make_interval(days => $1)",
                &[&KEPT_DAYS],
            )
            .await
            .context("forgetting old handed requests")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::with_test_pool;

    fn handed(agent: &str) -> Handed {
        Handed {
            id: Uuid::new_v4(),
            agent: agent.into(),
            agent_name: "allan".into(),
            place: "Slack".into(),
            words: "keep the video on track".into(),
            session: Some(format!("devbox/{}", Uuid::new_v4())),
            asked_at: Utc::now(),
            boss: None,
            delivered_at: None,
        }
    }

    #[tokio::test]
    async fn a_request_is_kept_once_delivered_to_its_boss_and_its_words_owed_until_sent() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let h = handed("driftwerk/allan");
            assert_eq!(store.handed_keep(&h).await.unwrap(), Kept::New);
            assert_eq!(
                store.handed_keep(&h).await.unwrap(),
                Kept::Undelivered,
                "a Desk that stopped before delivering it"
            );
            let same_id = Handed {
                agent: "other/hub".into(),
                ..h.clone()
            };
            assert_eq!(
                store.handed_keep(&same_id).await.unwrap(),
                Kept::New,
                "another agent's id is its own"
            );
            assert!(store.handed_of_boss("desk").await.unwrap().is_empty());

            let at = chrono::SubsecRound::trunc_subsecs(Utc::now(), 6);
            store.handed_delivered(&h, "desk", at).await.unwrap();
            let kept = store.handed_of_boss("desk").await.unwrap();
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].boss.as_deref(), Some("desk"));
            assert_eq!(kept[0].delivered_at, Some(at));
            assert_eq!(kept[0].session, h.session);
            assert_eq!(
                store.handed_keep(&h).await.unwrap(),
                Kept::Unanswered {
                    boss: "desk".into()
                }
            );

            store.handed_say(&h, "has it").await.unwrap();
            store.handed_say(&h, "stalled").await.unwrap();
            store.handed_say(&same_id, "refused").await.unwrap();
            assert_eq!(store.handed_keep(&h).await.unwrap(), Kept::Done);
            let owed = store.handed_owed(1).await.unwrap();
            assert_eq!(
                owed.iter()
                    .map(|o| (o.agent.as_str(), o.text.as_str()))
                    .collect::<Vec<_>>(),
                [("driftwerk/allan", "has it"), ("other/hub", "refused")],
                "one of each agent's at a time"
            );
            store.handed_sent(owed[0].id).await.unwrap();
            let left = store.handed_owed(10).await.unwrap();
            assert_eq!(left.len(), 2);
            assert_eq!(left[0].text, "stalled");

            for _ in 0..SAY_TRIES {
                store.handed_tried(left[0].id).await.unwrap();
            }
            let given_up = store.handed_owed(10).await.unwrap();
            assert_eq!(given_up.len(), 1, "tried too often");
            assert_eq!(given_up[0].text, "refused");

            let (a, b) = tokio::join!(
                store.handed_use(&h.agent, h.id),
                store.handed_use(&h.agent, h.id)
            );
            assert!(a.unwrap() ^ b.unwrap(), "claimed by one plan alone");
            assert!(!store.handed_use(&h.agent, h.id).await.unwrap());
            store.handed_unuse(&h.agent, h.id).await.unwrap();
            assert!(store.handed_use(&h.agent, h.id).await.unwrap(), "let go, it may be claimed again");

            assert_eq!(store.handed_prune().await.unwrap(), 0, "taken lately");
            store
                .pool()
                .get()
                .await
                .unwrap()
                .execute(
                    "UPDATE handed SET taken_at = now() - make_interval(days => $1 + 1) WHERE agent = $2",
                    &[&KEPT_DAYS, &same_id.agent],
                )
                .await
                .unwrap();
            assert_eq!(store.handed_prune().await.unwrap(), 1);
            assert!(store.handed_owed(10).await.unwrap().is_empty(), "its words went with it");
            assert_eq!(store.handed_keep(&h).await.unwrap(), Kept::Done);
        })
        .await;
    }
}
