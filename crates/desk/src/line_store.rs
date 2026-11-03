//! Desk's lines in Postgres (`line_messages`). A row is written whole on
//! every change; the moves two parties can race on (handing a row to
//! Desk's plugin, answering a question, the person editing a queued row)
//! are each one statement that checks the state it moves from.

use chrono::{DateTime, Utc};
use eyre::Result;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::crew::Party;
use crate::line::{Asked, Delivery, LineKind, LineMessage};
use crate::store::DeskStore;

const COLUMNS: &str = "id, at, sender, recipient, kind, text, in_reply_to, hop, delivery, \
    asked, escalation, group_id, delivered_at, updated_at";

fn message(row: &Row) -> Result<LineMessage> {
    let party = |c: &str| -> Result<Party> { Ok(serde_json::from_value(row.try_get(c)?)?) };
    Ok(LineMessage {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        from: party("sender")?,
        to: party("recipient")?,
        kind: LineKind::parse(row.try_get("kind")?)?,
        text: row.try_get("text")?,
        in_reply_to: row.try_get("in_reply_to")?,
        hop: row.try_get("hop")?,
        delivery: Delivery::parse(row.try_get("delivery")?)?,
        asked: row
            .try_get::<_, Option<&str>>("asked")?
            .map(Asked::parse)
            .transpose()?,
        escalation: row
            .try_get::<_, Option<serde_json::Value>>("escalation")?
            .and_then(|v| serde_json::from_value(v).ok()),
        group: row.try_get("group_id")?,
        delivered_at: row.try_get("delivered_at")?,
        updated_at: row.try_get("updated_at")?,
        waiting: None,
    })
}

/// Why answering a question didn't take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotAnswered {
    NoSuchAsk,
    /// Answered (or withdrawn) already: the first answer stands.
    Already,
}

impl DeskStore {
    /// Writes `m` whole, new or changed.
    pub async fn put_line(&self, m: &LineMessage) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                &format!(
                    "INSERT INTO line_messages ({COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
                     ON CONFLICT (id) DO UPDATE SET recipient = EXCLUDED.recipient, \
                     text = EXCLUDED.text, delivery = EXCLUDED.delivery, \
                     asked = EXCLUDED.asked, escalation = EXCLUDED.escalation, \
                     delivered_at = EXCLUDED.delivered_at, updated_at = EXCLUDED.updated_at"
                ),
                &[
                    &m.id,
                    &m.at,
                    &serde_json::to_value(&m.from)?,
                    &serde_json::to_value(&m.to)?,
                    &m.kind.as_str(),
                    &m.text,
                    &m.in_reply_to,
                    &m.hop,
                    &m.delivery.as_str(),
                    &m.asked.map(|a| a.as_str()),
                    &m.escalation
                        .as_ref()
                        .map(serde_json::to_value)
                        .transpose()?,
                    &m.group,
                    &m.delivered_at,
                    &m.updated_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn line_message(&self, id: Uuid) -> Result<Option<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {COLUMNS} FROM line_messages WHERE id = $1"),
                &[&id],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// The answer to `ask`, once there is one.
    pub async fn line_reply_to(&self, ask: Uuid) -> Result<Option<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages \
                     WHERE in_reply_to = $1 AND kind = 'answer' ORDER BY at LIMIT 1"
                ),
                &[&ask],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// The message to `recipient` (a party's id) whose marker is `mark`
    /// (the first characters of its id), newest first.
    pub async fn line_marked(&self, recipient: &str, mark: &str) -> Result<Option<LineMessage>> {
        let client = self.pool().get().await?;
        let like = format!("{}%", mark.to_lowercase());
        client
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages \
                     WHERE recipient ->> 'id' = $1 AND id::text LIKE $2 ORDER BY at DESC LIMIT 1"
                ),
                &[&recipient, &like],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// What waits to go to `recipient` (a party's id), oldest first.
    pub async fn line_inbox(&self, recipient: &str) -> Result<Vec<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages \
                     WHERE recipient ->> 'id' = $1 AND delivery IN ('queued', 'handed', 'held') \
                     ORDER BY at, id"
                ),
                &[&recipient],
            )
            .await?
            .iter()
            .map(message)
            .collect()
    }

    /// Everything waiting to go to a session, by session: what the pump
    /// starts from after a restart.
    pub async fn line_waiting(&self) -> Result<Vec<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages \
                     WHERE recipient ->> 'kind' = 'session' \
                     AND delivery IN ('queued', 'handed', 'held') ORDER BY at, id"
                ),
                &[],
            )
            .await?
            .iter()
            .map(message)
            .collect()
    }

    /// Hands a waiting row on: `Handed` to Desk's plugin, or `Delivered`.
    /// Whether this caller moved it (another may have first).
    pub async fn line_take(&self, id: Uuid, to: Delivery) -> Result<Option<LineMessage>> {
        let client = self.pool().get().await?;
        let delivered = (to == Delivery::Delivered).then(Utc::now);
        client
            .query_opt(
                &format!(
                    "UPDATE line_messages SET delivery = $2, \
                     handed_at = CASE WHEN $2 = 'handed' THEN now() ELSE handed_at END, \
                     delivered_at = COALESCE($3, delivered_at), updated_at = now() \
                     WHERE id = $1 AND delivery IN ('queued', 'held') RETURNING {COLUMNS}"
                ),
                &[&id, &to.as_str(), &delivered],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// Desk's plugin says the session has the handed row. `None` when it
    /// wasn't handed (a second copy that already ran).
    pub async fn line_ack(&self, id: Uuid) -> Result<Option<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!(
                    "UPDATE line_messages SET delivery = 'delivered', delivered_at = now(), \
                     updated_at = now() WHERE id = $1 AND delivery = 'handed' RETURNING {COLUMNS}"
                ),
                &[&id],
            )
            .await?
            .as_ref()
            .map(message)
            .transpose()
    }

    /// Puts handed rows back in the queue: one the plugin dropped (`id`),
    /// or every one handed longer ago than `before` (a plugin that died
    /// between taking and submitting it).
    pub async fn line_requeue(
        &self,
        id: Option<Uuid>,
        before: DateTime<Utc>,
    ) -> Result<Vec<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "UPDATE line_messages SET delivery = 'queued', handed_at = NULL, updated_at = now() \
                     WHERE delivery = 'handed' AND (id = $1 OR ($1 IS NULL AND handed_at < $2)) \
                     RETURNING {COLUMNS}"
                ),
                &[&id, &before],
            )
            .await?
            .iter()
            .map(message)
            .collect()
    }

    /// A session's thread: what it sent and what it was sent, newest
    /// first, before `before`.
    pub async fn line_thread(
        &self,
        session: Uuid,
        before: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<LineMessage>> {
        let client = self.pool().get().await?;
        let id = session.to_string();
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages \
                     WHERE ((sender ->> 'kind' = 'session' AND sender ->> 'id' = $1) \
                     OR (recipient ->> 'kind' = 'session' AND recipient ->> 'id' = $1)) \
                     AND ($2::timestamptz IS NULL OR at < $2) \
                     ORDER BY at DESC, id DESC LIMIT $3"
                ),
                &[&id, &before, &limit],
            )
            .await?
            .iter()
            .map(message)
            .collect()
    }

    /// Questions still waiting on an answer: asked of `recipient` (a
    /// party's id) when given, else all.
    pub async fn line_open_asks(&self, recipient: Option<&str>) -> Result<Vec<LineMessage>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages \
                     WHERE kind = 'ask' AND asked IN ('open', 'escalated') \
                     AND ($1::text IS NULL OR recipient ->> 'id' = $1) ORDER BY at"
                ),
                &[&recipient],
            )
            .await?
            .iter()
            .map(message)
            .collect()
    }

    /// Answers `ask` with `answer`, in one transaction: the first answer
    /// stands, a second is refused.
    pub async fn line_answer(
        &self,
        ask: Uuid,
        answer: &LineMessage,
    ) -> Result<Result<LineMessage, NotAnswered>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let Some(row) = tx
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM line_messages WHERE id = $1 AND kind = 'ask' FOR UPDATE"
                ),
                &[&ask],
            )
            .await?
        else {
            return Ok(Err(NotAnswered::NoSuchAsk));
        };
        let mut asked = message(&row)?;
        if !matches!(asked.asked, Some(Asked::Open | Asked::Escalated)) {
            return Ok(Err(NotAnswered::Already));
        }
        tx.execute(
            &format!(
                "INSERT INTO line_messages ({COLUMNS}) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"
            ),
            &[
                &answer.id,
                &answer.at,
                &serde_json::to_value(&answer.from)?,
                &serde_json::to_value(&answer.to)?,
                &answer.kind.as_str(),
                &answer.text,
                &answer.in_reply_to,
                &answer.hop,
                &answer.delivery.as_str(),
                &answer.asked.map(|a| a.as_str()),
                &answer
                    .escalation
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?,
                &answer.group,
                &answer.delivered_at,
                &answer.updated_at,
            ],
        )
        .await?;
        tx.execute(
            "UPDATE line_messages SET asked = 'answered', updated_at = now() WHERE id = $1",
            &[&ask],
        )
        .await?;
        tx.commit().await?;
        asked.asked = Some(Asked::Answered);
        Ok(Ok(asked))
    }

    /// The person changes (`Some`) or removes (`None`) a row of theirs that
    /// hasn't gone out. The row as it now is, or `None` once it was handed
    /// or delivered.
    pub async fn line_edit(&self, id: Uuid, text: Option<&str>) -> Result<Option<LineMessage>> {
        let client = self.pool().get().await?;
        let row = match text {
            Some(text) => {
                client
                    .query_opt(
                        &format!(
                            "UPDATE line_messages SET text = $2, updated_at = now() \
                             WHERE id = $1 AND sender ->> 'kind' = 'you' \
                             AND delivery IN ('queued', 'held') RETURNING {COLUMNS}"
                        ),
                        &[&id, &text],
                    )
                    .await?
            }
            None => {
                client
                    .query_opt(
                        &format!(
                            "DELETE FROM line_messages WHERE id = $1 AND sender ->> 'kind' = 'you' \
                             AND delivery IN ('queued', 'held') RETURNING {COLUMNS}"
                        ),
                        &[&id],
                    )
                    .await?
            }
        };
        row.as_ref().map(message).transpose()
    }

    /// When each session last had a message for the person on its line,
    /// and last exchanged one with anyone else: what its thread is unread
    /// by.
    pub async fn line_latest(
        &self,
    ) -> Result<Vec<(Uuid, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT s.id, \
                   max(m.at) FILTER (WHERE m.recipient ->> 'kind' = 'you') AS for_you, \
                   max(m.at) FILTER (WHERE m.recipient ->> 'kind' <> 'you' \
                     AND m.sender ->> 'kind' <> 'you') AS new \
                 FROM line_messages m \
                 CROSS JOIN LATERAL (VALUES (m.sender), (m.recipient)) AS p(party) \
                 JOIN sessions s ON p.party ->> 'kind' = 'session' AND s.id::text = p.party ->> 'id' \
                 GROUP BY s.id",
                &[],
            )
            .await?;
        rows.iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::store::DeskStore;

    fn ask(from: Party, to: Party) -> LineMessage {
        LineMessage::new(from, to, LineKind::Ask, "Which port?".into(), 1)
    }

    fn session_party(id: Uuid) -> Party {
        Party {
            kind: crate::crew::PartyKind::Session,
            id: id.to_string(),
            name: "acme-web".into(),
            engine: Some(crate::model::Engine::Claude),
            origin: None,
        }
    }

    #[tokio::test]
    async fn a_row_is_handed_once_and_a_dropped_or_stale_one_comes_back() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let to = Uuid::new_v4();
            let m = ask(Party::you(), session_party(to));
            s.put_line(&m).await.unwrap();
            assert_eq!(s.line_inbox(&to.to_string()).await.unwrap().len(), 1);
            assert!(s.line_take(m.id, Delivery::Handed).await.unwrap().is_some());
            assert!(
                s.line_take(m.id, Delivery::Handed).await.unwrap().is_none(),
                "taken once"
            );
            let back = s.line_requeue(Some(m.id), Utc::now()).await.unwrap();
            assert_eq!(back.len(), 1);
            assert_eq!(back[0].delivery, Delivery::Queued);
            s.line_take(m.id, Delivery::Handed).await.unwrap().unwrap();
            assert!(s
                .line_requeue(None, Utc::now() - chrono::Duration::minutes(2))
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                s.line_requeue(None, Utc::now() + chrono::Duration::seconds(1))
                    .await
                    .unwrap()
                    .len(),
                1
            );
            s.line_take(m.id, Delivery::Handed).await.unwrap().unwrap();
            assert_eq!(
                s.line_ack(m.id).await.unwrap().unwrap().delivery,
                Delivery::Delivered
            );
            assert!(
                s.line_ack(m.id).await.unwrap().is_none(),
                "a second copy changes nothing"
            );
            assert!(s.line_inbox(&to.to_string()).await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn the_first_answer_stands() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let asker = session_party(Uuid::new_v4());
            let q = ask(asker.clone(), Party::you());
            s.put_line(&q).await.unwrap();
            let reply = |text: &str| {
                let mut a = LineMessage::new(
                    Party::you(),
                    asker.clone(),
                    LineKind::Answer,
                    text.into(),
                    1,
                );
                a.in_reply_to = Some(q.id);
                a
            };
            let asked = s.line_answer(q.id, &reply("5433")).await.unwrap().unwrap();
            assert_eq!(asked.asked, Some(Asked::Answered));
            assert_eq!(
                s.line_answer(q.id, &reply("5432")).await.unwrap(),
                Err(NotAnswered::Already)
            );
            assert_eq!(
                s.line_answer(Uuid::new_v4(), &reply("x")).await.unwrap(),
                Err(NotAnswered::NoSuchAsk)
            );
            assert!(s.line_open_asks(None).await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn the_person_edits_or_removes_only_their_own_rows_and_only_until_they_go() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let s = DeskStore::with_pool(pool).await.unwrap();
            let session = session_party(Uuid::new_v4());
            let mine = LineMessage::new(
                Party::you(),
                session.clone(),
                LineKind::Tell,
                "run the tests".into(),
                0,
            );
            s.put_line(&mine).await.unwrap();
            let theirs = LineMessage::new(
                session.clone(),
                Party::you(),
                LineKind::Tell,
                "done".into(),
                1,
            );
            s.put_line(&theirs).await.unwrap();
            assert_eq!(
                s.line_edit(mine.id, Some("run the e2e tests"))
                    .await
                    .unwrap()
                    .unwrap()
                    .text,
                "run the e2e tests"
            );
            assert!(s.line_edit(theirs.id, Some("x")).await.unwrap().is_none());
            s.line_take(mine.id, Delivery::Handed)
                .await
                .unwrap()
                .unwrap();
            assert!(
                s.line_edit(mine.id, None).await.unwrap().is_none(),
                "on its way"
            );
            let thread = s
                .line_thread(session.id.parse().unwrap(), None, 10)
                .await
                .unwrap();
            assert_eq!(thread.len(), 2);
            assert_eq!(thread[0].id, theirs.id, "newest first");
        })
        .await;
    }
}
