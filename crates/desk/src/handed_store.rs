//! Handed requests and the words owed back for them, in Postgres
//! ([`crate::handed`]).

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::handed::{Handed, Owed};
use crate::store::DeskStore;

/// How long a delivered request is looked at for its plan's stalls.
const KEPT_DAYS: i32 = 30;

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
    /// Keeps `h` as taken, unless its id is kept already: whether it is new.
    pub async fn handed_keep(&self, h: &Handed) -> Result<bool> {
        let client = self.pool().get().await?;
        let made = client
            .execute(
                "INSERT INTO handed (id, agent, agent_name, place, words, session, asked_at, taken_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now()) ON CONFLICT (id) DO NOTHING",
                &[&h.id, &h.agent, &h.agent_name, &h.place, &h.words, &h.session, &h.asked_at],
            )
            .await
            .context("keeping a handed request")?;
        Ok(made == 1)
    }

    /// `id` delivered to `boss` now: when, as kept.
    pub async fn handed_delivered(&self, id: Uuid, boss: &str) -> Result<DateTime<Utc>> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "UPDATE handed SET boss = $2, delivered_at = now() WHERE id = $1 RETURNING delivered_at",
                &[&id, &boss],
            )
            .await
            .context("keeping where a handed request went")?;
        Ok(row.try_get(0)?)
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

    /// One word owed back for `handed`.
    pub async fn handed_say(&self, handed: Uuid, text: &str) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO handed_says (handed, text, at) VALUES ($1, $2, now())",
                &[&handed, &text],
            )
            .await
            .context("keeping a word owed for a handed request")?;
        Ok(())
    }

    /// The words owed and not yet sent, oldest first, at most `most`.
    pub async fn handed_owed(&self, most: i64) -> Result<Vec<Owed>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT s.id, s.handed, h.agent, s.text FROM handed_says s JOIN handed h ON h.id = s.handed \
                 WHERE s.sent_at IS NULL ORDER BY s.at, s.id LIMIT $1",
                &[&most],
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test]
    async fn a_request_is_kept_once_delivered_to_its_boss_and_its_words_owed_until_sent() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let h = Handed {
                id: Uuid::new_v4(),
                agent: "driftwerk/allan".into(),
                agent_name: "Allan".into(),
                place: "Slack".into(),
                words: "keep the video on track".into(),
                session: Some(Uuid::new_v4()),
                asked_at: Utc::now(),
                boss: None,
                delivered_at: None,
            };
            assert!(store.handed_keep(&h).await.unwrap());
            assert!(!store.handed_keep(&h).await.unwrap(), "once");
            assert!(store.handed_of_boss("desk").await.unwrap().is_empty());

            let at = store.handed_delivered(h.id, "desk").await.unwrap();
            let kept = store.handed_of_boss("desk").await.unwrap();
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].boss.as_deref(), Some("desk"));
            assert_eq!(kept[0].delivered_at, Some(at));
            assert_eq!(kept[0].words, h.words);

            store.handed_say(h.id, "has it").await.unwrap();
            store.handed_say(h.id, "stalled").await.unwrap();
            let owed = store.handed_owed(10).await.unwrap();
            assert_eq!(
                owed.iter().map(|o| o.text.as_str()).collect::<Vec<_>>(),
                ["has it", "stalled"]
            );
            assert!(owed
                .iter()
                .all(|o| o.agent == "driftwerk/allan" && o.handed == h.id));
            store.handed_sent(owed[0].id).await.unwrap();
            let left = store.handed_owed(10).await.unwrap();
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].text, "stalled");
        })
        .await;
    }
}
