//! The board in Postgres: `boards`, `tickets`, their needs and links. A
//! ticket is written whole on every change; its needs and links are rows
//! of their own.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde_json::Value;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::board::{Board, Link, Ticket, TicketState};
use crate::crew::Party;
use crate::store::DeskStore;

const BOARD: &str = "id, name, group_id, prefix, next_number, created_at, updated_at";
const TICKET: &str = "id, board, number, title, body, state, rank, assignee, created_by, \
                      pinned, snoozed_until, snoozed_at, state_at, created_at, updated_at, version";

fn board(row: &Row) -> Result<Board> {
    Ok(Board {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        group: row.try_get("group_id")?,
        prefix: row.try_get("prefix")?,
        next_number: row.try_get("next_number")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn party(v: Option<Value>) -> Result<Option<Party>> {
    v.filter(|v| !v.is_null())
        .map(serde_json::from_value)
        .transpose()
        .context("reading a ticket's party")
}

/// A ticket's own row; its needs and links are filled in after.
fn ticket(row: &Row) -> Result<Ticket> {
    Ok(Ticket {
        id: row.try_get("id")?,
        board: row.try_get("board")?,
        number: row.try_get("number")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        state: TicketState::parse(row.try_get("state")?)?,
        rank: row.try_get("rank")?,
        assignee: party(row.try_get("assignee")?)?,
        needs: vec![],
        links: vec![],
        created_by: party(Some(row.try_get("created_by")?))?
            .ok_or_else(|| eyre::eyre!("a ticket without its maker"))?,
        pinned: row.try_get("pinned")?,
        snoozed_until: row.try_get("snoozed_until")?,
        snoozed_at: row.try_get("snoozed_at")?,
        state_at: row.try_get("state_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        version: row.try_get("version")?,
    })
}

/// Why a ticket couldn't be written.
#[derive(Debug, thiserror::Error)]
pub enum TicketWrite {
    /// It changed since it was read: the write would undo that change.
    #[error("the ticket changed since it was read")]
    Stale,
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

impl From<tokio_postgres::Error> for TicketWrite {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Other(eyre::Report::new(e).wrap_err("writing a ticket"))
    }
}

impl From<workforce_postgres::PostgresError> for TicketWrite {
    fn from(e: workforce_postgres::PostgresError) -> Self {
        Self::Other(eyre::Report::new(e))
    }
}

impl From<serde_json::Error> for TicketWrite {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(eyre::Report::new(e))
    }
}

/// Why a board couldn't be written, in the person's words.
#[derive(Debug, thiserror::Error)]
pub enum BoardWrite {
    #[error("Another board's tickets already start with {0}; pick another prefix.")]
    PrefixTaken(String),
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

fn board_write(e: tokio_postgres::Error, prefix: Option<&str>) -> BoardWrite {
    match (e.code(), prefix) {
        (Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION), Some(p)) => {
            BoardWrite::PrefixTaken(p.to_string())
        }
        _ => BoardWrite::Other(eyre::Report::new(e).wrap_err("writing a board")),
    }
}

impl DeskStore {
    pub async fn boards(&self) -> Result<Vec<Board>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!("SELECT {BOARD} FROM boards ORDER BY created_at, id"),
                &[],
            )
            .await?
            .iter()
            .map(board)
            .collect()
    }

    pub async fn board(&self, id: Uuid) -> Result<Option<Board>> {
        let client = self.pool().get().await?;
        client
            .query_opt(&format!("SELECT {BOARD} FROM boards WHERE id = $1"), &[&id])
            .await?
            .as_ref()
            .map(board)
            .transpose()
    }

    pub async fn board_by_prefix(&self, prefix: &str) -> Result<Option<Board>> {
        let client = self.pool().get().await?;
        client
            .query_opt(
                &format!("SELECT {BOARD} FROM boards WHERE prefix = $1"),
                &[&prefix],
            )
            .await?
            .as_ref()
            .map(board)
            .transpose()
    }

    /// Writes `b`'s name, group and prefix, new or changed.
    pub async fn put_board(&self, b: &Board) -> Result<(), BoardWrite> {
        let client = self.pool().get().await.map_err(eyre::Report::new)?;
        client
            .execute(
                "INSERT INTO boards (id, name, group_id, prefix, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO UPDATE SET \
                 name = EXCLUDED.name, group_id = EXCLUDED.group_id, \
                 prefix = EXCLUDED.prefix, updated_at = EXCLUDED.updated_at",
                &[
                    &b.id,
                    &b.name,
                    &b.group,
                    &b.prefix,
                    &b.created_at,
                    &b.updated_at,
                ],
            )
            .await
            .map_err(|e| board_write(e, b.prefix.as_deref()))?;
        Ok(())
    }

    /// Deletes a board with its tickets; the tickets it had.
    pub async fn delete_board(&self, id: Uuid) -> Result<Vec<Uuid>> {
        let client = self.pool().get().await?;
        let rows = client
            .query("SELECT id FROM tickets WHERE board = $1", &[&id])
            .await?;
        client
            .execute("DELETE FROM boards WHERE id = $1", &[&id])
            .await?;
        rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }

    /// Adds `t` to its board under the board's next number, which is then
    /// used up: keys never repeat, even after a delete. `t.number` is set.
    pub async fn add_ticket(&self, t: &mut Ticket) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_one(
                "UPDATE boards SET next_number = next_number + 1 WHERE id = $1 \
                 RETURNING next_number - 1",
                &[&t.board],
            )
            .await
            .context("numbering a ticket")?;
        t.number = row.try_get(0)?;
        insert_ticket_in(&*tx, t).await?;
        for n in &t.needs {
            tx.execute(
                "INSERT INTO ticket_needs (ticket, needs) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&t.id, n],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Writes `t`'s own row as it is now (not its needs or links), if it
    /// is still as it was read; `t.version` moves on. Who works on it is
    /// written down with it: the session it is assigned to works on it
    /// from now while it is Doing or in Review, and no longer once it is
    /// taken off it, Done or Dropped.
    pub async fn put_ticket(&self, t: &mut Ticket) -> Result<(), TicketWrite> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "UPDATE tickets SET title = $3, body = $4, state = $5, rank = $6, \
                 assignee = $7, pinned = $8, snoozed_until = $9, snoozed_at = $10, \
                 state_at = $11, updated_at = $12, version = version + 1 \
                 WHERE id = $1 AND version = $2 RETURNING version",
                &[
                    &t.id,
                    &t.version,
                    &t.title,
                    &t.body,
                    &t.state.as_str(),
                    &t.rank,
                    &t.assignee.as_ref().map(serde_json::to_value).transpose()?,
                    &t.pinned,
                    &t.snoozed_until,
                    &t.snoozed_at,
                    &t.state_at,
                    &t.updated_at,
                ],
            )
            .await?;
        let Some(row) = row else {
            return Err(TicketWrite::Stale);
        };
        let working = t.session().filter(|_| crate::board::worked_on(t.state));
        let at = Utc::now();
        tx.execute(
            "UPDATE ticket_assignments SET until_at = $3 \
             WHERE ticket = $1 AND until_at IS NULL AND session IS DISTINCT FROM $2",
            &[&t.id, &working, &at],
        )
        .await?;
        tx.execute(
            "INSERT INTO ticket_assignments (ticket, session, from_at) \
             SELECT $1, $2, $3 WHERE $2::uuid IS NOT NULL AND NOT EXISTS ( \
               SELECT 1 FROM ticket_assignments WHERE ticket = $1 AND until_at IS NULL)",
            &[&t.id, &working, &at],
        )
        .await?;
        tx.commit().await?;
        t.version = row.try_get(0)?;
        Ok(())
    }

    pub async fn delete_ticket(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute("DELETE FROM tickets WHERE id = $1", &[&id])
            .await?;
        Ok(())
    }

    /// One ticket, with its needs and links.
    pub async fn ticket(&self, id: Uuid) -> Result<Option<Ticket>> {
        let client = self.pool().get().await?;
        let Some(row) = client
            .query_opt(
                &format!("SELECT {TICKET} FROM tickets WHERE id = $1"),
                &[&id],
            )
            .await?
        else {
            return Ok(None);
        };
        let mut t = vec![ticket(&row)?];
        // Not held while its needs and links are read on another.
        drop(client);
        self.fill(&mut t).await?;
        Ok(t.pop())
    }

    pub async fn ticket_by_number(&self, board: Uuid, number: i32) -> Result<Option<Ticket>> {
        let client = self.pool().get().await?;
        let id: Option<Uuid> = client
            .query_opt(
                "SELECT id FROM tickets WHERE board = $1 AND number = $2",
                &[&board, &number],
            )
            .await?
            .map(|r| r.try_get(0))
            .transpose()?;
        // Not held while the ticket is read on another: two of these at
        // once on a small pool would each wait for the other's.
        drop(client);
        match id {
            Some(id) => self.ticket(id).await,
            None => Ok(None),
        }
    }

    /// Every ticket of `board`, in column order.
    pub async fn board_tickets(&self, board: Uuid) -> Result<Vec<Ticket>> {
        self.tickets_where("board = $1", &[&board]).await
    }

    /// The tickets assigned to the session `id`.
    pub async fn session_tickets(&self, id: Uuid) -> Result<Vec<Ticket>> {
        self.tickets_where(
            "assignee ->> 'kind' = 'session' AND assignee ->> 'id' = $1",
            &[&id.to_string()],
        )
        .await
    }

    /// The tickets linked to any of the sessions `ids`.
    pub async fn linked_tickets(&self, ids: &[Uuid]) -> Result<Vec<Ticket>> {
        let targets: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        self.tickets_where(
            "id IN (SELECT ticket FROM ticket_links WHERE kind = 'session' AND target = ANY($1))",
            &[&targets],
        )
        .await
    }

    /// The tickets whose title and body match `query` (a `to_tsquery`), or
    /// whose key is `key`, at most `limit`, latest changed first.
    pub async fn search_tickets(
        &self,
        query: Option<&str>,
        key: Option<(Uuid, i32)>,
        limit: i64,
    ) -> Result<Vec<Ticket>> {
        let (board, number) = key.unzip();
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {TICKET} FROM tickets WHERE \
                     ($1::text IS NOT NULL AND to_tsvector('simple', title || ' ' || body) \
                        @@ to_tsquery('simple', $1)) \
                     OR (board = $2 AND number = $3) \
                     ORDER BY (board = $2 AND number = $3) IS TRUE DESC, updated_at DESC \
                     LIMIT $4"
                ),
                &[&query, &board, &number, &limit],
            )
            .await
            .context("searching tickets")?;
        let mut out = rows.iter().map(ticket).collect::<Result<Vec<_>>>()?;
        drop(client);
        self.fill(&mut out).await?;
        Ok(out)
    }

    async fn tickets_where(
        &self,
        wher: &str,
        args: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<Ticket>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!("SELECT {TICKET} FROM tickets WHERE {wher} ORDER BY state, rank, number"),
                args,
            )
            .await?;
        let mut out = rows.iter().map(ticket).collect::<Result<Vec<_>>>()?;
        drop(client);
        self.fill(&mut out).await?;
        Ok(out)
    }

    /// Fills in the needs and links of `tickets`.
    async fn fill(&self, tickets: &mut [Ticket]) -> Result<()> {
        if tickets.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = tickets.iter().map(|t| t.id).collect();
        let client = self.pool().get().await?;
        let mut needs: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for r in client
            .query(
                "SELECT ticket, needs FROM ticket_needs WHERE ticket = ANY($1) \
                 ORDER BY ticket, needs",
                &[&ids],
            )
            .await?
        {
            needs.entry(r.try_get(0)?).or_default().push(r.try_get(1)?);
        }
        let mut links: HashMap<Uuid, Vec<Link>> = HashMap::new();
        for r in client
            .query(
                "SELECT ticket, kind, target, detail FROM ticket_links WHERE ticket = ANY($1) \
                 ORDER BY at, kind, target",
                &[&ids],
            )
            .await?
        {
            let detail: Option<Value> = r.try_get(3)?;
            if let Some(link) = Link::read(r.try_get(1)?, r.try_get(2)?, detail.as_ref()) {
                links.entry(r.try_get(0)?).or_default().push(link);
            }
        }
        for t in tickets {
            t.needs = needs.remove(&t.id).unwrap_or_default();
            t.links = links.remove(&t.id).unwrap_or_default();
        }
        Ok(())
    }

    /// How many tickets the session `id` added since `since`.
    pub async fn tickets_made_by(&self, id: Uuid, since: DateTime<Utc>) -> Result<i64> {
        let client = self.pool().get().await?;
        Ok(client
            .query_one(
                "SELECT count(*) FROM tickets WHERE created_by ->> 'kind' = 'session' \
                 AND created_by ->> 'id' = $1 AND created_at > $2",
                &[&id.to_string(), &since],
            )
            .await?
            .try_get(0)?)
    }

    /// Every dependency there is, as `(ticket, needs)`.
    pub async fn ticket_edges(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let client = self.pool().get().await?;
        client
            .query("SELECT ticket, needs FROM ticket_needs", &[])
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// The state of each ticket of `ids`.
    pub async fn ticket_states(&self, ids: &[Uuid]) -> Result<HashMap<Uuid, TicketState>> {
        let client = self.pool().get().await?;
        client
            .query("SELECT id, state FROM tickets WHERE id = ANY($1)", &[&ids])
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, TicketState::parse(r.try_get(1)?)?)))
            .collect()
    }

    /// The key of each ticket of `ids` whose board has a prefix.
    pub async fn ticket_keys(&self, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT t.id, b.prefix || '-' || t.number FROM tickets t \
                 JOIN boards b ON b.id = t.board WHERE t.id = ANY($1) AND b.prefix IS NOT NULL",
                &[&ids],
            )
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// Whether `needs` was added (false: it was there already).
    pub async fn put_need(&self, ticket: Uuid, needs: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "INSERT INTO ticket_needs (ticket, needs) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&ticket, &needs],
            )
            .await?
            > 0)
    }

    /// Whether `needs` was there to remove.
    pub async fn delete_need(&self, ticket: Uuid, needs: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "DELETE FROM ticket_needs WHERE ticket = $1 AND needs = $2",
                &[&ticket, &needs],
            )
            .await?
            > 0)
    }

    /// Links `ticket` to `link` from `at`; a link made again moves to `at`
    /// (a session assigned again works on it from then).
    pub async fn put_link(&self, ticket: Uuid, link: &Link, at: DateTime<Utc>) -> Result<()> {
        let (kind, target, detail) = link.stored();
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO ticket_links (ticket, kind, target, detail, at) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (ticket, kind, target) \
                 DO UPDATE SET detail = EXCLUDED.detail, at = EXCLUDED.at",
                &[&ticket, &kind, &target, &detail, &at],
            )
            .await?;
        Ok(())
    }

    /// Spaces out the ranks of `board`'s `state` column, keeping its order:
    /// for a drop between two cards with no room between them.
    pub async fn space_column(&self, board: Uuid, state: TicketState, step: f64) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE tickets t SET rank = o.n::float8 * $3 FROM ( \
                   SELECT id, row_number() OVER (ORDER BY rank, number) AS n \
                   FROM tickets WHERE board = $1 AND state = $2) o \
                 WHERE t.id = o.id",
                &[&board, &state.as_str(), &step],
            )
            .await?;
        Ok(())
    }
}

async fn insert_ticket_in<C: tokio_postgres::GenericClient>(client: &C, t: &Ticket) -> Result<()> {
    client
        .execute(
            &format!(
                "INSERT INTO tickets ({TICKET}) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"
            ),
            &[
                &t.id,
                &t.board,
                &t.number,
                &t.title,
                &t.body,
                &t.state.as_str(),
                &t.rank,
                &t.assignee.as_ref().map(serde_json::to_value).transpose()?,
                &serde_json::to_value(&t.created_by)?,
                &t.pinned,
                &t.snoozed_until,
                &t.snoozed_at,
                &t.state_at,
                &t.created_at,
                &t.updated_at,
                &t.version,
            ],
        )
        .await
        .context("writing a ticket")?;
    Ok(())
}
