//! The boss's picture in Postgres (`picture_events`): each row written as
//! it happens, read back after a restart (the last day, and facts and
//! orders for longer), and the oldest let go.

use chrono::{DateTime, Utc};
use eyre::Result;
use tokio_postgres::Row;

use crate::picture::{Happened, What, FACTS_FOR, KEPT_FOR};
use crate::store::DeskStore;

/// How long a row is kept in Postgres at all.
const KEPT_IN_STORE: chrono::TimeDelta = chrono::TimeDelta::days(30);

const COLUMNS: &str = "seq, at, what, boss, session, sessions, text, turn";

fn happened(r: &Row) -> Result<Happened> {
    let what: String = r.try_get(2)?;
    let seq: i64 = r.try_get(0)?;
    Ok(Happened {
        seq: u64::try_from(seq)?,
        at: r.try_get(1)?,
        what: What::parse(&what).ok_or_else(|| eyre::eyre!("unknown picture row {what}"))?,
        boss: r.try_get(3)?,
        session: r.try_get(4)?,
        sessions: r.try_get(5)?,
        text: r.try_get(6)?,
        turn: r.try_get(7)?,
    })
}

impl DeskStore {
    /// Keeps `h`. Written once: the same number again is left as it is.
    pub async fn picture_put(&self, h: &Happened) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO picture_events ({COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                     ON CONFLICT (seq) DO NOTHING"
                ),
                &[
                    &i64::try_from(h.seq)?,
                    &h.at,
                    &h.what.as_str(),
                    &h.boss,
                    &h.session,
                    &h.sessions,
                    &h.text,
                    &h.turn,
                ],
            )
            .await?;
        Ok(())
    }

    /// What the picture holds as of `now`, oldest first, and the largest
    /// number ever given (numbering goes on after it). Rows past keeping
    /// go first.
    pub async fn picture_load(&self, now: DateTime<Utc>) -> Result<(Vec<Happened>, u64)> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM picture_events WHERE at < $1",
                &[&(now - KEPT_IN_STORE)],
            )
            .await?;
        let top: Option<i64> = client
            .query_one("SELECT MAX(seq) FROM picture_events", &[])
            .await?
            .try_get(0)?;
        let rows = client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM picture_events \
                     WHERE at >= $1 OR (what IN ('fact', 'order') AND at >= $2) ORDER BY seq"
                ),
                &[&(now - KEPT_FOR), &(now - FACTS_FOR)],
            )
            .await?;
        let rows = rows.iter().map(happened).collect::<Result<Vec<_>>>()?;
        Ok((rows, top.map_or(0, |t| u64::try_from(t).unwrap_or(0))))
    }
}

#[cfg(test)]
mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::picture::{Note, Picture};

    #[tokio::test]
    async fn rows_round_trip_and_numbering_goes_on() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let store = DeskStore::with_pool(pool).await.unwrap();
            let s = uuid::Uuid::new_v4();
            let now = Utc::now();
            let mut p = Picture::default();
            let a = p
                .push(
                    Note::new(What::Instruction, "Open the PR.")
                        .boss("boss")
                        .session(s)
                        .turn(Some("t1".into())),
                    now,
                )
                .unwrap();
            let b = p
                .push(
                    Note::new(What::Fact, "Sam owns the fix.")
                        .boss("boss")
                        .sessions(vec![s]),
                    now,
                )
                .unwrap();
            let old = Happened {
                seq: 1000,
                at: now - KEPT_FOR - chrono::TimeDelta::hours(1),
                what: What::PersonSaid,
                boss: None,
                session: Some(s),
                sessions: vec![],
                text: "an old word".into(),
                turn: None,
            };
            for h in [&a, &b, &old, &a] {
                store.picture_put(h).await.unwrap();
            }
            let (rows, top) = store.picture_load(now).await.unwrap();
            assert_eq!(top, 1000, "the largest number given, kept or not");
            // Postgres keeps microseconds: compared by what each says.
            let said: Vec<_> = rows
                .iter()
                .map(|r| {
                    (
                        r.seq,
                        r.what,
                        r.text.clone(),
                        r.turn.clone(),
                        r.sessions.clone(),
                    )
                })
                .collect();
            assert_eq!(
                said,
                vec![
                    (a.seq, a.what, a.text.clone(), a.turn.clone(), vec![]),
                    (b.seq, b.what, b.text.clone(), None, vec![s]),
                ],
                "the day's rows, once each"
            );
            assert_eq!(rows[0].session, Some(s));
            let mut again = Picture::default();
            assert!(again.load(rows, top).is_empty());
            let next = again
                .push(Note::new(What::PersonSaid, "x").session(s), now)
                .unwrap();
            assert_eq!(next.seq, 1001);
        })
        .await;
    }
}
