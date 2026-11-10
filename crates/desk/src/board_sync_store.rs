//! Board sync in Postgres: the hubs a board can sync through
//! (`sync_hubs`), each board's link (`boards.sync`), synced tickets found
//! by their issue's key, and the push outbox (`board_outbox`). No token is
//! ever here: the person's token for a hub is in the hub's vault.

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::Serialize;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::board::{Sync, System, Ticket};
use crate::board_store::json_of;
use crate::store::DeskStore;

/// A hub a board can sync through: its id here, its name and URL, and the
/// person whose token it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHub {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(skip)]
    pub person: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn sync_hub(row: &Row) -> Result<SyncHub> {
    Ok(SyncHub {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        url: row.try_get("url")?,
        person: row.try_get("person")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// A ticket waiting for its issue to hear of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub ticket: Uuid,
    pub board: Uuid,
    pub due_at: DateTime<Utc>,
    pub tries: i32,
    pub creating_at: Option<DateTime<Utc>>,
    pub waiting: Option<String>,
    pub error: Option<String>,
    pub queued_at: DateTime<Utc>,
}

fn queued(row: &Row) -> Result<Queued> {
    Ok(Queued {
        ticket: row.try_get("ticket")?,
        board: row.try_get("board")?,
        due_at: row.try_get("due_at")?,
        tries: row.try_get("tries")?,
        creating_at: row.try_get("creating_at")?,
        waiting: row.try_get("waiting")?,
        error: row.try_get("error")?,
        queued_at: row.try_get("queued_at")?,
    })
}

impl DeskStore {
    pub async fn sync_hubs(&self) -> Result<Vec<SyncHub>> {
        let client = self.pool().get().await?;
        client
            .query("SELECT * FROM sync_hubs ORDER BY name, id", &[])
            .await?
            .iter()
            .map(sync_hub)
            .collect()
    }

    pub async fn sync_hub(&self, id: &str) -> Result<Option<SyncHub>> {
        let client = self.pool().get().await?;
        client
            .query_opt("SELECT * FROM sync_hubs WHERE id = $1", &[&id])
            .await?
            .as_ref()
            .map(sync_hub)
            .transpose()
    }

    pub async fn put_sync_hub(&self, h: &SyncHub) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO sync_hubs (id, name, url, person, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO UPDATE SET \
                 name = EXCLUDED.name, url = EXCLUDED.url, person = EXCLUDED.person, \
                 updated_at = EXCLUDED.updated_at",
                &[
                    &h.id,
                    &h.name,
                    &h.url,
                    &h.person,
                    &h.created_at,
                    &h.updated_at,
                ],
            )
            .await
            .context("keeping a hub to sync through")?;
        Ok(())
    }

    /// Whether there was such a hub to forget.
    pub async fn delete_sync_hub(&self, id: &str) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute("DELETE FROM sync_hubs WHERE id = $1", &[&id])
            .await?
            > 0)
    }

    /// Changes board `id`'s link with `change`, in one step: what the sync
    /// loop and the person write never overwrite each other. `None` when
    /// there's no such board. The prefix is set to `prefix` with it.
    pub async fn change_board_sync(
        &self,
        id: Uuid,
        change: impl FnOnce(Option<Sync>, Option<String>) -> Result<(Option<Sync>, Option<String>)>,
    ) -> Result<Option<(Option<Sync>, Option<String>)>> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let Some(row) = tx
            .query_opt(
                "SELECT sync, prefix FROM boards WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?
        else {
            return Ok(None);
        };
        let sync: Option<Sync> = json_of(row.try_get("sync")?)?;
        let prefix: Option<String> = row.try_get("prefix")?;
        let (sync, prefix) = change(sync, prefix)?;
        tx.execute(
            "UPDATE boards SET sync = $2, prefix = $3, updated_at = now() WHERE id = $1",
            &[
                &id,
                &sync.as_ref().map(serde_json::to_value).transpose()?,
                &prefix,
            ],
        )
        .await
        .context("writing a board's link")?;
        tx.commit().await?;
        Ok(Some((sync, prefix)))
    }

    /// The board other than `not` linked to `system`'s `target` through
    /// `via`, if any: two clients' hubs may each have a project of one key.
    pub async fn board_linked_to(
        &self,
        system: System,
        target: &str,
        via: &crate::board::Via,
        not: Uuid,
    ) -> Result<Option<Uuid>> {
        let client = self.pool().get().await?;
        let via = serde_json::to_value(via)?;
        client
            .query_opt(
                "SELECT id FROM boards WHERE sync ->> 'system' = $1 AND sync ->> 'target' = $2 \
                 AND sync -> 'via' = $4 AND id <> $3 LIMIT 1",
                &[&system.as_str(), &target, &not, &via],
            )
            .await?
            .map(|r| r.try_get(0))
            .transpose()
            .map_err(Into::into)
    }

    /// The ticket the link `link` holds for `system`'s issue `key`.
    pub async fn ticket_by_external(
        &self,
        system: System,
        link: Uuid,
        key: &str,
    ) -> Result<Option<Ticket>> {
        let client = self.pool().get().await?;
        let id: Option<Uuid> = client
            .query_opt(
                "SELECT id FROM tickets WHERE external ->> 'system' = $1 AND external ->> 'key' = $2 \
                 AND external ->> 'link' = $3",
                &[&system.as_str(), &key, &link.to_string()],
            )
            .await?
            .map(|r| r.try_get(0))
            .transpose()?;
        drop(client);
        match id {
            Some(id) => self.ticket(id).await,
            None => Ok(None),
        }
    }

    /// Every ticket whose issue's key is `key`, on any system and link,
    /// latest changed first: two links may each hold one (two clients' hubs
    /// with a project of one key), so the caller picks the one it reaches.
    pub async fn tickets_by_issue_key(&self, key: &str) -> Result<Vec<Ticket>> {
        let client = self.pool().get().await?;
        let ids: Vec<Uuid> = client
            .query(
                "SELECT id FROM tickets WHERE external ->> 'key' = $1 ORDER BY updated_at DESC",
                &[&key],
            )
            .await?
            .iter()
            .map(|r| r.try_get(0))
            .collect::<std::result::Result<_, _>>()?;
        drop(client);
        let mut out = vec![];
        for id in ids {
            if let Some(t) = self.ticket(id).await? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// Queues `ticket` on `board` to be pushed from `at`. A ticket already
    /// queued keeps its place, and its back-off when it has failed.
    pub async fn queue_push(&self, ticket: Uuid, board: Uuid, at: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO board_outbox (ticket, board, due_at, queued_at) VALUES ($1, $2, $3, $3) \
                 ON CONFLICT (ticket) DO UPDATE SET board = EXCLUDED.board, \
                 queued_at = GREATEST(board_outbox.queued_at, EXCLUDED.queued_at), \
                 due_at = CASE WHEN board_outbox.tries > 0 OR board_outbox.waiting IS NOT NULL \
                               THEN board_outbox.due_at ELSE LEAST(board_outbox.due_at, EXCLUDED.due_at) END",
                &[&ticket, &board, &at],
            )
            .await
            .context("queueing a ticket's push")?;
        Ok(())
    }

    /// The tickets of `board` due to be pushed at `now`, oldest first.
    pub async fn due_pushes(
        &self,
        board: Uuid,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Queued>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT * FROM board_outbox WHERE board = $1 AND due_at <= $2 \
                 ORDER BY due_at, queued_at LIMIT $3",
                &[&board, &now, &limit],
            )
            .await?
            .iter()
            .map(queued)
            .collect()
    }

    /// Every ticket of `board` waiting to be pushed.
    pub async fn queued_pushes(&self, board: Uuid) -> Result<Vec<Queued>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT * FROM board_outbox WHERE board = $1 ORDER BY due_at, queued_at",
                &[&board],
            )
            .await?
            .iter()
            .map(queued)
            .collect()
    }

    /// `ticket`'s push went out as of `queued_at`: off the outbox, unless it
    /// was queued again meanwhile.
    pub async fn pushed(&self, ticket: Uuid, as_of: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "DELETE FROM board_outbox WHERE ticket = $1 AND queued_at <= $2",
                &[&ticket, &as_of],
            )
            .await?;
        client
            .execute(
                "UPDATE board_outbox SET tries = 0, creating_at = NULL, waiting = NULL, error = NULL \
                 WHERE ticket = $1",
                &[&ticket],
            )
            .await?;
        Ok(())
    }

    /// A create of `ticket`'s issue is being sent: if no answer is kept, it
    /// is looked for before it is sent again.
    pub async fn creating(&self, ticket: Uuid, at: DateTime<Utc>) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE board_outbox SET creating_at = COALESCE(creating_at, $2) WHERE ticket = $1",
                &[&ticket, &at],
            )
            .await?;
        Ok(())
    }

    /// `ticket`'s push didn't go: tried again from `next`, with `error` (or
    /// waiting for the person's approval, with `waiting`) said.
    pub async fn push_failed(
        &self,
        ticket: Uuid,
        next: DateTime<Utc>,
        error: Option<&str>,
        waiting: Option<&str>,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE board_outbox SET tries = tries + 1, due_at = $2, error = $3, waiting = $4 \
                 WHERE ticket = $1",
                &[&ticket, &next, &error, &waiting],
            )
            .await?;
        Ok(())
    }

    /// Takes `ticket` off the outbox.
    pub async fn unqueue_push(&self, ticket: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute("DELETE FROM board_outbox WHERE ticket = $1", &[&ticket])
            .await?;
        Ok(())
    }

    /// Every board with a link, for the sync loop.
    pub async fn synced_boards(&self) -> Result<Vec<Uuid>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id FROM boards WHERE sync IS NOT NULL ORDER BY created_at, id",
                &[],
            )
            .await?
            .iter()
            .map(|r| Ok(r.try_get(0)?))
            .collect()
    }

    /// Adds `t`, an issue the pull imported, under the board's next number;
    /// `false` when another ticket already has that issue (it stays there).
    pub async fn import_ticket(&self, t: &mut Ticket) -> Result<bool> {
        let mut client = self.pool().get().await?;
        let mut tx = client.transaction().await?;
        let row = tx
            .query_one(
                "UPDATE boards SET next_number = next_number + 1 WHERE id = $1 \
                 RETURNING next_number - 1",
                &[&t.board],
            )
            .await
            .context("numbering an imported ticket")?;
        t.number = row.try_get(0)?;
        let sp = tx.savepoint("import").await?;
        match crate::board_store::insert_ticket_in(&*sp, t).await {
            Ok(()) => {
                sp.commit().await?;
                tx.commit().await?;
                Ok(true)
            }
            Err(e) => {
                let taken = e
                    .chain()
                    .filter_map(|c| c.downcast_ref::<tokio_postgres::Error>())
                    .any(|pg| {
                        pg.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
                    });
                match taken {
                    true => Ok(false),
                    false => Err(e),
                }
            }
        }
    }
}
