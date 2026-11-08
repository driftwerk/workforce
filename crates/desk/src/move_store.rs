//! Moves and handover notes in Postgres (desk migration `handovers`).

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::crew::Party;
use crate::moves::{Handover, HandoverState, Item, MoveRow, MoveState};
use crate::store::DeskStore;

const HANDOVER_COLUMNS: &str = "id, move, item, from_group, to_group, from_boss, to_boss, note, \
     drafted_by, asked_until, edited, state, created_at, delivered_at";

fn handover_of(r: &Row) -> Result<Handover> {
    Ok(Handover {
        id: r.try_get(0)?,
        move_id: r.try_get(1)?,
        item: r.try_get(2)?,
        from_group: r.try_get(3)?,
        to_group: r.try_get(4)?,
        from_boss: r.try_get(5)?,
        to_boss: r.try_get(6)?,
        note: r.try_get(7)?,
        drafted_by: serde_json::from_value(r.try_get(8)?)?,
        asked_until: r.try_get(9)?,
        edited: r.try_get(10)?,
        state: HandoverState::parse(r.try_get(11)?)?,
        created_at: r.try_get(12)?,
        delivered_at: r.try_get(13)?,
    })
}

fn move_of(r: &Row) -> Result<MoveRow> {
    Ok(MoveRow {
        id: r.try_get(0)?,
        item: Item::parse(r.try_get(1)?)
            .ok_or_else(|| eyre::eyre!("a move of an item Desk can't read"))?,
        from_group: r.try_get(2)?,
        to_group: r.try_get(3)?,
        choices: serde_json::from_value(r.try_get(4)?)?,
        effects: serde_json::from_value(r.try_get(5)?)?,
        state: MoveState::parse(r.try_get(6)?)?,
        expires_at: r.try_get(7)?,
        created_at: r.try_get(8)?,
        done_at: r.try_get(9)?,
    })
}

/// A note as the person sends it with the move: which, the words, and
/// whether they changed it from the draft.
#[derive(Debug, Clone)]
pub struct Sent {
    pub id: Uuid,
    pub note: String,
    pub edited: bool,
}

impl DeskStore {
    /// Keeps a previewed move and its draft notes, together.
    pub async fn add_move(&self, m: &MoveRow, handovers: &[Handover]) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let item = m.item.key();
        tx.execute(
            "INSERT INTO moves (id, item, from_group, to_group, choices, effects, state, \
               expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &m.id,
                &item,
                &m.from_group,
                &m.to_group,
                &serde_json::to_value(m.choices)?,
                &serde_json::to_value(&m.effects)?,
                &m.state.as_str(),
                &m.expires_at,
                &m.created_at,
            ],
        )
        .await
        .context("keeping a move's preview")?;
        for h in handovers {
            tx.execute(
                "INSERT INTO handovers (id, move, item, from_group, to_group, from_boss, \
                   to_boss, note, drafted_by, asked_until, edited, state, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    &h.id,
                    &h.move_id,
                    &h.item,
                    &h.from_group,
                    &h.to_group,
                    &h.from_boss,
                    &h.to_boss,
                    &h.note,
                    &serde_json::to_value(&h.drafted_by)?,
                    &h.asked_until,
                    &h.edited,
                    &h.state.as_str(),
                    &h.created_at,
                ],
            )
            .await
            .context("keeping a handover's draft")?;
        }
        tx.commit().await.context("keeping a move's preview")?;
        Ok(())
    }

    /// The move `id`, as previewed.
    pub async fn move_row(&self, id: Uuid) -> Result<Option<MoveRow>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                "SELECT id, item, from_group, to_group, choices, effects, state, \
                   expires_at, created_at, done_at FROM moves WHERE id = $1",
                &[&id],
            )
            .await?;
        row.as_ref().map(move_of).transpose()
    }

    /// The notes of the move `id`, oldest first.
    pub async fn handovers_of(&self, id: Uuid) -> Result<Vec<Handover>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {HANDOVER_COLUMNS} FROM handovers WHERE move = $1 \
                     ORDER BY created_at, id"
                ),
                &[&id],
            )
            .await?
            .iter()
            .map(handover_of)
            .collect()
    }

    /// The note `id`.
    pub async fn handover(&self, id: Uuid) -> Result<Option<Handover>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                &format!("SELECT {HANDOVER_COLUMNS} FROM handovers WHERE id = $1"),
                &[&id],
            )
            .await?;
        row.as_ref().map(handover_of).transpose()
    }

    /// The old boss `boss` drafts note `id`: only its own, still a draft of
    /// a move still previewed, and while it is asked to.
    pub async fn draft_handover(
        &self,
        id: Uuid,
        boss: &Party,
        note: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Handover>> {
        let client = self.pool().get().await?;
        let row = client
            .query_opt(
                &format!(
                    "UPDATE handovers h SET note = $3, drafted_by = $4 \
                     WHERE h.id = $1 AND h.from_boss = $2 AND h.state = 'draft' \
                       AND h.asked_until IS NOT NULL AND $5 < h.asked_until \
                       AND EXISTS (SELECT 1 FROM moves m WHERE m.id = h.move \
                         AND m.state = 'previewed' AND $5 < m.expires_at) \
                     RETURNING {HANDOVER_COLUMNS}"
                ),
                &[&id, &boss.id, &note, &serde_json::to_value(boss)?, &now],
            )
            .await
            .context("keeping a boss's handover draft")?;
        row.as_ref().map(handover_of).transpose()
    }

    /// The person makes the move `id`: once, while its preview stands. Its
    /// notes are kept as sent and delivered; any it had that weren't sent
    /// are discarded. `None` when it was made, dropped or ran out already.
    pub async fn finish_move(
        &self,
        id: Uuid,
        sent: &[Sent],
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<Handover>>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let claimed = tx
            .execute(
                "UPDATE moves SET state = 'done', done_at = $2 \
                 WHERE id = $1 AND state = 'previewed' AND $2 < expires_at",
                &[&id, &now],
            )
            .await?;
        if claimed == 0 {
            return Ok(None);
        }
        let mut delivered = Vec::new();
        for s in sent {
            let row = tx
                .query_opt(
                    &format!(
                        "UPDATE handovers SET note = $3, edited = $4, state = 'delivered', \
                           delivered_at = $5 \
                         WHERE id = $1 AND move = $2 AND state = 'draft' \
                         RETURNING {HANDOVER_COLUMNS}"
                    ),
                    &[&s.id, &id, &s.note, &s.edited, &now],
                )
                .await?;
            if let Some(r) = row {
                delivered.push(handover_of(&r)?);
            }
        }
        tx.execute(
            "UPDATE handovers SET state = 'discarded' WHERE move = $1 AND state = 'draft'",
            &[&id],
        )
        .await?;
        tx.commit().await.context("making a move")?;
        Ok(Some(delivered))
    }
}

impl DeskStore {
    /// Sets the move `id` aside, made or not, with its notes: it failed.
    pub async fn drop_move(&self, id: Uuid) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute("UPDATE moves SET state = 'dropped' WHERE id = $1", &[&id])
            .await?;
        tx.execute(
            "UPDATE handovers SET state = 'discarded' WHERE move = $1",
            &[&id],
        )
        .await?;
        tx.commit().await.context("setting a failed move aside")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moves::{Choices, Effect};
    use crate::store::build_pool;
    use workforce_postgres::test_support::with_test_pool;

    fn preview(now: DateTime<Utc>) -> (MoveRow, Handover) {
        let id = Uuid::new_v4();
        let m = MoveRow {
            id,
            item: Item::Session(Uuid::from_u128(1)),
            from_group: Some("g-acme".into()),
            to_group: Some("g-atlas".into()),
            choices: Choices::default(),
            effects: vec![Effect::Tell],
            state: MoveState::Previewed,
            expires_at: now + crate::moves::PREVIEW_FOR,
            created_at: now,
            done_at: None,
        };
        let h = Handover {
            id: Uuid::new_v4(),
            move_id: id,
            item: m.item.key(),
            from_group: m.from_group.clone(),
            to_group: m.to_group.clone(),
            from_boss: Some("sarah".into()),
            to_boss: Some("mia".into()),
            note: "Handing over acme-api.".into(),
            drafted_by: Party::desk(),
            asked_until: Some(now + chrono::Duration::seconds(60)),
            edited: false,
            state: HandoverState::Draft,
            created_at: now,
            delivered_at: None,
        };
        (m, h)
    }

    #[tokio::test]
    async fn a_note_is_drafted_by_its_own_boss_only_and_delivered_once_as_sent() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let now = crate::crew::now();
            let (m, h) = preview(now);
            store.add_move(&m, std::slice::from_ref(&h)).await.unwrap();
            assert_eq!(store.move_row(m.id).await.unwrap().unwrap(), m);
            let mia = Party::manager("mia", "Mia");
            assert!(
                store
                    .draft_handover(h.id, &mia, "Atlas's words", now)
                    .await
                    .unwrap()
                    .is_none(),
                "another boss can't write it"
            );
            let sarah = Party::manager("sarah", "Sarah");
            let drafted = store
                .draft_handover(h.id, &sarah, "Sarah's draft", now)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(drafted.note, "Sarah's draft");
            assert_eq!(drafted.drafted_by, sarah);
            assert!(
                store
                    .draft_handover(h.id, &sarah, "late", now + chrono::Duration::seconds(61))
                    .await
                    .unwrap()
                    .is_none(),
                "too late to draft"
            );
            let sent = [Sent {
                id: h.id,
                note: "The person's words".into(),
                edited: true,
            }];
            let delivered = store.finish_move(m.id, &sent, now).await.unwrap().unwrap();
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].note, "The person's words");
            assert!(delivered[0].edited);
            assert_eq!(delivered[0].state, HandoverState::Delivered);
            assert!(
                store.finish_move(m.id, &sent, now).await.unwrap().is_none(),
                "made once"
            );
            assert!(store
                .draft_handover(h.id, &sarah, "after", now)
                .await
                .unwrap()
                .is_none());

            // A preview that ran out can't be made.
            let (old, oh) = preview(now - chrono::Duration::minutes(11));
            store.add_move(&old, &[oh]).await.unwrap();
            assert!(store.finish_move(old.id, &[], now).await.unwrap().is_none());
        })
        .await;
    }
}
