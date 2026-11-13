//! The board ([`crate::board`]) as the desk runs it: boards and tickets
//! changed by the person through Desk, by sessions through their line's
//! `ticket` tool, and by Desk itself when a ticket follows its pull
//! request. Each change is stored, recorded in Activity (the ticket's
//! history) and announced as `{type: "ticket"}`, `"ticketRemoved"`,
//! `"board"` or `"boardRemoved"`.
//!
//! Assigning a ticket to a session links the two, moves the ticket to
//! Doing, makes the session's goal the ticket unless it had one, and tells
//! the session on its line; from then on the ledger stamps its calls with
//! the ticket.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Desk, DeskError, Sender};
use crate::board::{self, Board, Link, Pr, Ticket, TicketCall, TicketState};
use crate::board_store::{BoardWrite, TicketWrite};
use crate::crew::{self, ActivityPage, By, Kind, Party, PartyKind};
use crate::crew_store::History;
use crate::line::{Send, Target, When};
use crate::model::{DeskEvent, Engine, NewSession, SessionPatch, SessionState, SessionView};

/// A ticket as Desk shows it: its key, and what the board computes of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketView {
    #[serde(flatten)]
    pub ticket: Ticket,
    pub key: Option<String>,
    /// Todo with every need done: it may be handed out.
    pub ready: bool,
    /// Its needs not yet done.
    pub blockers: Vec<Uuid>,
    /// Done long enough, or Dropped: on the settled shelf.
    pub settled: bool,
    /// Hidden from the board until it wakes.
    pub snoozed: bool,
    /// Its issue left the board's import filter: on the Left-the-filter
    /// shelf, for the person to keep or drop.
    pub left_filter: bool,
    /// What stops its change going out to its issue, in plain words.
    pub push_problem: Option<String>,
    /// Its files: its issue's, or what the person dropped on it.
    pub files: Vec<crate::ticket_files::TicketFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBoard {
    pub name: String,
    /// Its sidebar group; none is ungrouped.
    #[serde(default)]
    pub group: Option<String>,
    /// What its keys start with; from its name when not said.
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardPatch {
    pub name: Option<String>,
    /// `null` makes it ungrouped.
    #[serde(default, deserialize_with = "crate::model::present")]
    pub group: Option<Option<String>>,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTicket {
    pub board: Uuid,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Its column; Backlog when not said.
    #[serde(default)]
    pub state: Option<TicketState>,
    #[serde(default)]
    pub needs: Vec<Uuid>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub pinned: Option<bool>,
    /// Until when it is hidden from the board; `null` wakes it.
    #[serde(default, deserialize_with = "crate::model::present")]
    pub snoozed_until: Option<Option<DateTime<Utc>>>,
}

/// A drop in a column: `before` is the card it lands after (the one above
/// it), `after` the card it lands before (the one below it); neither puts
/// it at the end.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Move {
    pub state: TicketState,
    #[serde(default)]
    pub before: Option<Uuid>,
    #[serde(default)]
    pub after: Option<Uuid>,
}

/// Whom a ticket is assigned to: `{ "session": id }`, `{ "agent": id }`
/// or `{ "newSession": { "path", "engine" } }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AssignTo {
    Session(Uuid),
    Agent(String),
    NewSession(NewSessionFor),
}

/// A session to start for a ticket.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NewSessionFor {
    pub path: String,
    pub engine: Engine,
}

/// The boss a session started for a ticket is in the charge of, and the
/// sidebar group it starts in.
#[derive(Debug, Clone)]
pub struct ChargedTo {
    pub boss: Party,
    pub group: Option<String>,
}

/// Which tickets of a board a list holds.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TicketQuery {
    #[serde(default)]
    pub state: Option<TicketState>,
    /// A session's or an agent's id, or `none` for the unassigned.
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
}

/// How many tickets a search gives at most.
const SEARCH_MAX: i64 = 20;

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

fn written(e: BoardWrite) -> DeskError {
    match e {
        BoardWrite::PrefixTaken(_) => DeskError::Conflict(e.to_string()),
        BoardWrite::Other(e) => DeskError::Internal(e),
    }
}

fn stale(e: TicketWrite) -> DeskError {
    match e {
        TicketWrite::Stale => DeskError::Conflict(
            "That ticket changed while this was being done; look at it again.".into(),
        ),
        TicketWrite::Other(e) => DeskError::Internal(e),
    }
}

fn now() -> DateTime<Utc> {
    crew::now()
}

/// `t` in `to`, changed now.
fn set_state(t: &mut Ticket, to: TicketState, at: DateTime<Utc>) {
    if t.state != to {
        t.state = to;
        t.state_at = at;
    }
    t.updated_at = at;
}

impl Desk {
    fn board_crew(&self) -> Option<std::sync::Arc<dyn crew::Crew>> {
        self.0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    /// Writes `t` as changed from what was read of it; a change made
    /// meanwhile by anyone else is never overwritten.
    pub(super) async fn save_ticket(&self, t: &mut Ticket) -> Result<(), DeskError> {
        self.0.store.put_ticket(t).await.map_err(stale)
    }

    // ── Boards ───────────────────────────────────────────────────────────

    pub async fn boards(&self) -> Result<Vec<Board>, DeskError> {
        self.0.store.boards().await.map_err(internal)
    }

    /// The sidebar group the board `id` is in (none: ungrouped).
    pub async fn group_of_board(&self, id: Uuid) -> Result<Option<String>, DeskError> {
        Ok(self.board_of(id).await?.group)
    }

    pub(super) async fn board_of(&self, id: Uuid) -> Result<Board, DeskError> {
        self.0
            .store
            .board(id)
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)
    }

    /// The sidebar group `group` names, checked against the layout: a
    /// board's group is one the person has.
    fn board_group(&self, group: Option<String>) -> Result<Option<String>, DeskError> {
        let Some(g) = group
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
        else {
            return Ok(None);
        };
        match self.layout().groups.iter().any(|x| x.id == g) {
            true => Ok(Some(g)),
            false => Err(DeskError::Invalid(format!(
                "There's no group {g:?} in the sidebar."
            ))),
        }
    }

    pub async fn create_board(&self, new: NewBoard) -> Result<Board, DeskError> {
        let name = new.name.trim().to_string();
        if name.is_empty() {
            return Err(DeskError::Invalid("A board needs a name.".into()));
        }
        let prefix = match new.prefix.map(|p| p.trim().to_ascii_uppercase()) {
            Some(p) if !p.is_empty() => {
                board::check_prefix(&p).map_err(DeskError::Invalid)?;
                p
            }
            _ => board::prefix_of(&name),
        };
        let at = now();
        let b = Board {
            id: Uuid::new_v4(),
            name,
            group: self.board_group(new.group)?,
            prefix: Some(prefix),
            sync: None,
            next_number: 1,
            created_at: at,
            updated_at: at,
        };
        self.0.store.put_board(&b).await.map_err(written)?;
        self.announce(DeskEvent::Board {
            board: Box::new(b.clone()),
        });
        Ok(b)
    }

    /// Boards whose sidebar group was deleted, now ungrouped: every Desk
    /// hears of them.
    pub(super) async fn boards_ungrouped(&self, ids: &[Uuid]) {
        for id in ids {
            match self.0.store.board(*id).await {
                Ok(Some(b)) => self.announce(DeskEvent::Board { board: Box::new(b) }),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(board = %id, error = format!("{e:#}"), "couldn't read an ungrouped board")
                }
            }
        }
    }

    pub async fn update_board(&self, id: Uuid, patch: BoardPatch) -> Result<Board, DeskError> {
        let mut b = self.board_of(id).await?;
        if let Some(name) = patch.name {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(DeskError::Invalid("A board needs a name.".into()));
            }
            b.name = name;
        }
        if let Some(group) = patch.group {
            let group = self.board_group(group)?;
            // A link belongs to one group: its issues never go to another.
            if group != b.group && b.sync.is_some() {
                return Err(DeskError::Conflict(format!(
                    "{} is synced with {}; unlink it before moving it to another group.",
                    b.name,
                    b.sync.as_ref().map_or("", |s| s.target.as_str())
                )));
            }
            b.group = group;
        }
        if let Some(p) = patch.prefix.filter(|_| b.sync.is_none()) {
            let p = p.trim().to_ascii_uppercase();
            if b.prefix.as_deref() != Some(p.as_str()) {
                board::check_prefix(&p).map_err(DeskError::Invalid)?;
                // Keys stay what they were: in Activity, in sessions'
                // goals, in what was said about them.
                if b.numbered() {
                    return Err(DeskError::Conflict(format!(
                        "{} has had tickets, so its keys stay {}-….",
                        b.name,
                        b.prefix.as_deref().unwrap_or_default()
                    )));
                }
                b.prefix = Some(p);
            }
        }
        b.updated_at = now();
        self.0.store.put_board(&b).await.map_err(written)?;
        self.announce(DeskEvent::Board {
            board: Box::new(b.clone()),
        });
        Ok(b)
    }

    /// Deletes a board and its tickets; the sessions working on them are
    /// told.
    pub async fn delete_board(&self, id: Uuid, by: &By) -> Result<(), DeskError> {
        let b = self.board_of(id).await?;
        let tickets = self.0.store.board_tickets(id).await.map_err(internal)?;
        for t in &tickets {
            self.tell_gone(&b, t, by).await;
        }
        self.0.store.delete_board(id).await.map_err(internal)?;
        for t in tickets {
            self.announce(DeskEvent::TicketRemoved { id: t.id });
        }
        self.announce(DeskEvent::BoardRemoved { id });
        Ok(())
    }

    // ── Reading tickets ──────────────────────────────────────────────────

    /// `tickets` as Desk shows them, with what the board computes of each.
    pub(super) async fn views(&self, tickets: Vec<Ticket>) -> Result<Vec<TicketView>, DeskError> {
        let mut states: HashMap<Uuid, TicketState> =
            tickets.iter().map(|t| (t.id, t.state)).collect();
        let missing: Vec<Uuid> = tickets
            .iter()
            .flat_map(|t| t.needs.iter())
            .filter(|n| !states.contains_key(n))
            .copied()
            .collect();
        if !missing.is_empty() {
            states.extend(
                self.0
                    .store
                    .ticket_states(&missing)
                    .await
                    .map_err(internal)?,
            );
        }
        let boards: HashMap<Uuid, Board> = self
            .boards()
            .await?
            .into_iter()
            .map(|b| (b.id, b))
            .collect();
        let ids: Vec<Uuid> = tickets.iter().map(|t| t.id).collect();
        let mut problems = self.0.store.push_problems(&ids).await.map_err(internal)?;
        let mut files = self.0.store.files_of(&ids).await.map_err(internal)?;
        let at = now();
        Ok(tickets
            .into_iter()
            .map(|t| TicketView {
                push_problem: problems.remove(&t.id),
                files: files.remove(&t.id).unwrap_or_default(),
                key: boards.get(&t.board).and_then(|b| t.key(b)),
                ready: board::ready(&t, &states),
                blockers: board::blockers(&t, &states),
                settled: board::settled(&t, at),
                snoozed: board::snoozed(&t, at),
                left_filter: t.left_filter(),
                ticket: t,
            })
            .collect())
    }

    pub(super) async fn view_ticket(&self, t: Ticket) -> Result<TicketView, DeskError> {
        Ok(self.views(vec![t]).await?.remove(0))
    }

    pub(super) async fn ticket_of(&self, id: Uuid) -> Result<Ticket, DeskError> {
        self.0
            .store
            .ticket(id)
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)
    }

    pub async fn ticket(&self, id: Uuid) -> Result<TicketView, DeskError> {
        let t = self.ticket_of(id).await?;
        self.view_ticket(t).await
    }

    /// `board`'s tickets that `q` asks for.
    pub async fn tickets(
        &self,
        board: Uuid,
        q: &TicketQuery,
    ) -> Result<Vec<TicketView>, DeskError> {
        self.board_of(board).await?;
        let mut tickets = self.0.store.board_tickets(board).await.map_err(internal)?;
        let matched = match q.q.as_deref().and_then(board::search_query) {
            Some(query) => Some(
                self.0
                    .store
                    .search_tickets(Some(&query), None, i64::MAX)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .map(|t| t.id)
                    .collect::<std::collections::HashSet<_>>(),
            ),
            None => None,
        };
        let mut views = self.views(std::mem::take(&mut tickets)).await?;
        views.retain(|v| {
            let t = &v.ticket;
            q.state.is_none_or(|s| t.state == s)
                && match q.assignee.as_deref() {
                    None => true,
                    Some("none") => t.assignee.is_none(),
                    Some(id) => t.assignee.as_ref().is_some_and(|a| a.id == id),
                }
                && matched.as_ref().is_none_or(|m| m.contains(&t.id))
        });
        Ok(views)
    }

    /// Tickets for the palette: by their words, or by their key.
    pub async fn search_tickets(&self, q: &str) -> Result<Vec<TicketView>, DeskError> {
        let key = match board::parse_key(q) {
            Some((prefix, n)) => self
                .0
                .store
                .board_by_prefix(&prefix)
                .await
                .map_err(internal)?
                .map(|b| (b.id, n)),
            None => None,
        };
        let query = board::search_query(q);
        // Every board's ticket for a synced key: the person's palette sees
        // all their boards, and two links may each hold the key.
        let synced = match crate::board_sync::parse_issue_key(q) {
            Some(k) => self
                .0
                .store
                .tickets_by_issue_key(&k)
                .await
                .map_err(internal)?,
            None => vec![],
        };
        if query.is_none() && key.is_none() && synced.is_empty() {
            return Ok(vec![]);
        }
        let mut found = self
            .0
            .store
            .search_tickets(query.as_deref(), key, SEARCH_MAX)
            .await
            .map_err(internal)?;
        found.retain(|f| !synced.iter().any(|t| t.id == f.id));
        found.splice(0..0, synced);
        self.views(found).await
    }

    /// A page of a ticket's history, newest first; a deleted ticket's
    /// stays.
    pub async fn ticket_history(
        &self,
        id: Uuid,
        before: Option<&str>,
        limit: i64,
    ) -> Result<ActivityPage, DeskError> {
        self.history(History::Ticket(id), before, limit).await
    }

    /// A page of what a session or an agent did and had done to it.
    pub async fn history_of(
        &self,
        subject: &str,
        before: Option<&str>,
        limit: i64,
    ) -> Result<ActivityPage, DeskError> {
        self.history(History::Of(subject.to_string()), before, limit)
            .await
    }

    async fn history(
        &self,
        of: History,
        before: Option<&str>,
        limit: i64,
    ) -> Result<ActivityPage, DeskError> {
        let (events, more) = self
            .0
            .store
            .history(&of, before, limit.clamp(1, 200))
            .await
            .map_err(internal)?;
        Ok(ActivityPage { events, more })
    }

    // ── Changing tickets ─────────────────────────────────────────────────

    /// Records `kind` by `by` on `t`, about `subject`, and announces `t`.
    pub(super) async fn ticket_changed(
        &self,
        t: Ticket,
        by: &By,
        kind: Kind,
        subject: Option<Party>,
        text: String,
    ) -> Result<TicketView, DeskError> {
        let b = self.board_of(t.board).await.ok();
        let group = b.as_ref().and_then(|b| b.group.clone());
        self.note(
            by.event(kind, subject, text)
                .about_ticket(t.id, group.as_deref()),
        );
        // A change here on a synced board goes out to its issue; the pull's
        // own changes don't go back.
        let sync = b.as_ref().and_then(|b| b.sync.as_ref());
        let pushes = sync.is_some()
            && !crate::board_sync::is_remote_party(&by.party)
            && match &t.external {
                Some(e) => e.synced && !e.kept,
                // Only what the person made here becomes an issue there, and
                // never one the board had before it was linked keeping them.
                None => {
                    t.created_by.kind == PartyKind::You
                        && sync
                            .and_then(|s| s.local_before)
                            .is_none_or(|at| t.created_at > at)
                }
            };
        if pushes {
            if let Err(e) = self.0.store.queue_push(t.id, t.board, now()).await {
                tracing::warn!(ticket = %t.id, error = format!("{e:#}"), "a synced ticket's change wasn't queued to go out");
            }
        }
        let v = self.view_ticket(t).await?;
        self.announce(DeskEvent::Ticket {
            ticket: Box::new(v.clone()),
        });
        Ok(v)
    }

    pub(super) async fn key_of(&self, t: &Ticket) -> Result<String, DeskError> {
        let b = self.board_of(t.board).await?;
        Ok(t.key(&b).unwrap_or_else(|| "a ticket".into()))
    }

    /// The rank of a card dropped at the end of `state` on `board`.
    pub(super) async fn end_rank(
        &self,
        board: Uuid,
        state: TicketState,
        not: Uuid,
    ) -> Result<f64, DeskError> {
        let tickets = self.0.store.board_tickets(board).await.map_err(internal)?;
        let last = tickets
            .iter()
            .filter(|t| t.state == state && t.id != not)
            .map(|t| t.rank)
            .fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r))));
        Ok(board::rank_between(last, None).unwrap_or(board::RANK_STEP))
    }

    /// `t` moved to the end of `to`, when it isn't in `to` already.
    pub(super) async fn to_column(
        &self,
        t: &mut Ticket,
        to: TicketState,
        at: DateTime<Utc>,
    ) -> Result<(), DeskError> {
        if t.state != to {
            t.rank = self.end_rank(t.board, to, t.id).await?;
        }
        set_state(t, to, at);
        Ok(())
    }

    pub async fn create_ticket(&self, new: NewTicket, by: &By) -> Result<TicketView, DeskError> {
        let title = new.title.trim().to_string();
        if title.is_empty() {
            return Err(DeskError::Invalid("A ticket needs a title.".into()));
        }
        let b = self.board_of(new.board).await?;
        if !new.needs.is_empty() {
            let known = self
                .0
                .store
                .ticket_states(&new.needs)
                .await
                .map_err(internal)?;
            if let Some(gone) = new.needs.iter().find(|n| !known.contains_key(n)) {
                return Err(DeskError::Invalid(format!(
                    "There's no ticket {gone} to need."
                )));
            }
        }
        let state = new.state.unwrap_or(TicketState::Backlog);
        let at = now();
        let mut t = Ticket {
            id: Uuid::new_v4(),
            board: b.id,
            number: 0,
            title,
            body: new.body.trim().to_string(),
            state,
            rank: self.end_rank(b.id, state, Uuid::nil()).await?,
            assignee: None,
            needs: new.needs,
            links: vec![],
            external: None,
            created_by: by.party.clone(),
            pinned: false,
            snoozed_until: None,
            snoozed_at: None,
            state_at: at,
            created_at: at,
            updated_at: at,
            version: 0,
        };
        self.0.store.add_ticket(&mut t).await.map_err(internal)?;
        let key = t.key(&b).unwrap_or_default();
        let text = format!("created {key} “{}”", crew::one_line(&t.title, 120));
        self.ticket_changed(t, by, Kind::TicketCreated, None, text)
            .await
    }

    /// The person's edit: what it says, its pin and its snooze. A pin or a
    /// snooze doesn't change the ticket, so it doesn't wake it.
    pub async fn edit_ticket(
        &self,
        id: Uuid,
        patch: TicketPatch,
        by: &By,
    ) -> Result<TicketView, DeskError> {
        let mut t = self.ticket_of(id).await?;
        let at = now();
        let mut said = vec![];
        if let Some(title) = patch.title.map(|s| s.trim().to_string()) {
            if title.is_empty() {
                return Err(DeskError::Invalid("A ticket needs a title.".into()));
            }
            if title != t.title {
                said.push(format!("renamed it “{}”", crew::one_line(&title, 120)));
                t.title = title;
            }
        }
        if let Some(body) = patch.body.map(|s| s.trim().to_string()) {
            if body != t.body {
                said.push("edited its description".to_string());
                t.body = body;
            }
        }
        if !said.is_empty() {
            t.updated_at = at;
        }
        let quiet = said.is_empty();
        if let Some(pinned) = patch.pinned {
            t.pinned = pinned;
        }
        if let Some(until) = patch.snoozed_until {
            t.snoozed_until = until;
            t.snoozed_at = until.map(|_| at);
        }
        self.save_ticket(&mut t).await?;
        if quiet {
            let v = self.view_ticket(t).await?;
            self.announce(DeskEvent::Ticket {
                ticket: Box::new(v.clone()),
            });
            return Ok(v);
        }
        let key = self.key_of(&t).await?;
        let text = format!("{} on {key}", said.join(" and "));
        self.ticket_changed(t, by, Kind::TicketEdited, None, text)
            .await
    }

    /// A drop anywhere in a column.
    pub async fn move_ticket(&self, id: Uuid, mv: Move, by: &By) -> Result<TicketView, DeskError> {
        let t = self.ticket_of(id).await?;
        self.move_read(t, mv, by).await
    }

    /// Moves `t` as it was read: a change made to it since is never undone.
    /// Anyone but a session dropping it or putting it back in the queue
    /// takes it off whoever had it, and that session is told.
    pub(super) async fn move_read(
        &self,
        mut t: Ticket,
        mv: Move,
        by: &By,
    ) -> Result<TicketView, DeskError> {
        let rank = self.drop_rank(&t, &mv).await?;
        let from = t.state;
        set_state(&mut t, mv.state, now());
        t.rank = rank;
        let off = match by.party.kind != PartyKind::Session && board::unassigns(mv.state) {
            true => t.assignee.take(),
            false => None,
        };
        self.save_ticket(&mut t).await?;
        if let Some(session) = off.as_ref().and_then(crate::line::session_of) {
            let key = self.key_of(&t).await?;
            let text = format!(
                "{key} was moved to {}; it is no longer yours. Stop working on it unless you are asked again.",
                t.state.title()
            );
            self.tell_session(session, text, by).await;
        }
        let why = off.as_ref().map(|p| format!("taken off {}", p.name));
        self.ticket_moved_about(t, from, by, why.as_deref(), off)
            .await
    }

    async fn ticket_moved(
        &self,
        t: Ticket,
        from: TicketState,
        by: &By,
        why: Option<&str>,
    ) -> Result<TicketView, DeskError> {
        self.ticket_moved_about(t, from, by, why, None).await
    }

    /// `ticket_moved`, about `was` when it was taken off them.
    async fn ticket_moved_about(
        &self,
        t: Ticket,
        from: TicketState,
        by: &By,
        why: Option<&str>,
        was: Option<Party>,
    ) -> Result<TicketView, DeskError> {
        let key = self.key_of(&t).await?;
        let text = match (from == t.state, why) {
            (true, _) => format!("reordered {key} in {}", t.state.title()),
            (false, None) => format!("moved {key} to {}", t.state.title()),
            (false, Some(why)) => format!("moved {key} to {}: {why}", t.state.title()),
        };
        let subject = t.assignee.clone().or(was);
        self.ticket_changed(t, by, Kind::TicketMoved, subject, text)
            .await
    }

    /// The rank `t` takes dropped where `mv` says, spacing the column out
    /// when there is no room.
    async fn drop_rank(&self, t: &Ticket, mv: &Move) -> Result<f64, DeskError> {
        for _ in 0..2 {
            let mut column: Vec<Ticket> = self
                .0
                .store
                .board_tickets(t.board)
                .await
                .map_err(internal)?
                .into_iter()
                .filter(|c| c.state == mv.state && c.id != t.id)
                .collect();
            column.sort_by(|a, b| a.rank.total_cmp(&b.rank).then(a.number.cmp(&b.number)));
            let at = |id: Uuid| {
                column.iter().position(|c| c.id == id).ok_or_else(|| {
                    DeskError::Invalid(format!(
                        "That card isn't in {} on this board.",
                        mv.state.title()
                    ))
                })
            };
            let (before, after) = match (mv.before, mv.after) {
                (Some(b), _) => {
                    let i = at(b)?;
                    (Some(column[i].rank), column.get(i + 1).map(|c| c.rank))
                }
                (None, Some(a)) => {
                    let i = at(a)?;
                    (
                        i.checked_sub(1).map(|j| column[j].rank),
                        Some(column[i].rank),
                    )
                }
                (None, None) => (column.last().map(|c| c.rank), None),
            };
            match board::rank_between(before, after) {
                Some(r) => return Ok(r),
                None => self
                    .0
                    .store
                    .space_column(t.board, mv.state, board::RANK_STEP)
                    .await
                    .map_err(internal)?,
            }
        }
        Err(DeskError::Conflict(
            "The column changed while the card moved; drop it again.".into(),
        ))
    }

    /// `id` needs `other` (or no longer does); a cycle is refused.
    pub async fn need_ticket(
        &self,
        id: Uuid,
        other: Uuid,
        needs: bool,
        by: &By,
    ) -> Result<TicketView, DeskError> {
        let mut t = self.ticket_of(id).await?;
        let o = self.ticket_of(other).await?;
        let (key, other_key) = (self.key_of(&t).await?, self.key_of(&o).await?);
        let changed = match needs {
            true => {
                if id == other {
                    return Err(DeskError::Invalid("A ticket can't need itself.".into()));
                }
                let edges = self.0.store.ticket_edges().await.map_err(internal)?;
                if board::cycles(id, other, &edges) {
                    return Err(DeskError::Conflict(format!(
                        "{other_key} already needs {key}."
                    )));
                }
                self.0.store.put_need(id, other).await.map_err(internal)?
            }
            false => self
                .0
                .store
                .delete_need(id, other)
                .await
                .map_err(internal)?,
        };
        if !changed {
            return self.view_ticket(t).await;
        }
        t.updated_at = now();
        self.save_ticket(&mut t).await?;
        let t = self.ticket_of(id).await?;
        let text = match needs {
            true => format!("made {key} need {other_key}"),
            false => format!("made {key} no longer need {other_key}"),
        };
        self.ticket_changed(t, by, Kind::TicketEdited, None, text)
            .await
    }

    /// Assigns a ticket to a session, an agent or a new session, or to no
    /// one (`None`).
    pub async fn assign_ticket(
        &self,
        id: Uuid,
        to: Option<AssignTo>,
        by: &By,
    ) -> Result<TicketView, DeskError> {
        self.assign_ticket_for(id, to, by, None).await
    }

    /// `assign_ticket`, where a new session is one a boss runs from the
    /// start: in its charge, and in its group, before it runs.
    pub async fn assign_ticket_for(
        &self,
        id: Uuid,
        to: Option<AssignTo>,
        by: &By,
        charged: Option<&ChargedTo>,
    ) -> Result<TicketView, DeskError> {
        let t = self.ticket_of(id).await?;
        let b = self.board_of(t.board).await?;
        let key = t.key(&b).unwrap_or_else(|| "a ticket".into());
        match to {
            None => self.unassign(t, &key, by).await,
            Some(AssignTo::Session(session)) => {
                let view = self.view(session)?;
                self.assign_session(t, &key, view, by, true).await
            }
            Some(AssignTo::NewSession(new)) => {
                let needs = self.need_keys(&t).await?;
                let prompt = board::handed_text(&key, &t, &needs);
                // The boss's group, else the board's while the sidebar has
                // it: its session works where the boss or the board is.
                let group = charged.and_then(|c| c.group.clone()).or_else(|| {
                    b.group
                        .clone()
                        .filter(|g| self.layout().groups.iter().any(|x| &x.id == g))
                });
                let session = NewSession {
                    fresh_worktree: false,
                    engine: new.engine,
                    view: None,
                    name: key.clone(),
                    cwd: new.path,
                    goal: board::goal_of(&key, &t),
                    worktree: None,
                    prompt: Some(prompt),
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    account: None,
                    env: BTreeMap::new(),
                    group,
                };
                let view = match charged {
                    Some(c) => self.create_for(session, &c.boss, by.clone()).await?,
                    None => self.create(session, Some(by.clone())).await?,
                };
                // Its prompt is the ticket: no message on its line too. The
                // ticket is read again: it may have changed while the
                // session started, and is never started for twice.
                let name = view.label().to_string();
                let started = view.id;
                let given = match self.ticket_of(id).await {
                    Ok(t) => self.assign_session(t, &key, view, by, false).await,
                    Err(e) => Err(e),
                };
                given.map_err(|e| {
                    DeskError::Conflict(format!(
                        "The session {name} ({started}) did start for {key}, but the ticket couldn't be given to it ({e}). It is running: don't start another."
                    ))
                })
            }
            Some(AssignTo::Agent(agent)) => self.assign_agent(t, &key, &agent, by).await,
        }
    }

    /// The ticket `key` names (`ACM-12`).
    pub async fn ticket_by_key(&self, key: &str) -> Result<TicketView, DeskError> {
        self.ticket_by_key_in(key, |_| true).await
    }

    /// [`Self::ticket_by_key`] among the boards whose group `sees` takes: a
    /// synced key two links hold names the one in reach.
    pub async fn ticket_by_key_in(
        &self,
        key: &str,
        sees: impl Fn(Option<&str>) -> bool,
    ) -> Result<TicketView, DeskError> {
        if let Some(t) = self
            .synced_by_key(key, |b| sees(b.group.as_deref()))
            .await?
        {
            return self.view_ticket(t).await;
        }
        let (prefix, n) = board::parse_key(key).ok_or_else(|| {
            DeskError::Invalid(format!("{key:?} isn't a ticket's key, like ACM-12."))
        })?;
        let b = self
            .0
            .store
            .board_by_prefix(&prefix)
            .await
            .map_err(internal)?
            .ok_or_else(|| DeskError::Invalid(format!("No board has keys starting {prefix}.")))?;
        let t = self
            .0
            .store
            .ticket_by_number(b.id, n)
            .await
            .map_err(internal)?
            .ok_or_else(|| DeskError::Invalid(format!("There's no {prefix}-{n}.")))?;
        self.view_ticket(t).await
    }

    /// The synced ticket whose issue's key is `key` (`acme-482` as typed).
    pub(super) async fn synced_by_key(
        &self,
        key: &str,
        reaches: impl Fn(&Board) -> bool,
    ) -> Result<Option<Ticket>, DeskError> {
        let Some(key) = crate::board_sync::parse_issue_key(key) else {
            return Ok(None);
        };
        for t in self
            .0
            .store
            .tickets_by_issue_key(&key)
            .await
            .map_err(internal)?
        {
            if reaches(&self.board_of(t.board).await?) {
                return Ok(Some(t));
            }
        }
        Ok(None)
    }

    /// The key of the ticket assigned to `session` that is in Doing, when
    /// there is one: a turn that ends with it still there has left it
    /// unfinished or unmoved.
    pub async fn doing_key(&self, session: Uuid) -> Option<String> {
        let tickets = match self.0.store.session_tickets(session).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(%session, error = %e, "a session's tickets weren't read");
                return None;
            }
        };
        let t = tickets
            .into_iter()
            .filter(|t| t.state == TicketState::Doing)
            .max_by_key(|t| t.updated_at)?;
        let b = self.board_of(t.board).await.ok()?;
        t.key(&b)
    }

    /// The keys and states of what `t` needs.
    pub(super) async fn need_keys(
        &self,
        t: &Ticket,
    ) -> Result<Vec<(String, TicketState)>, DeskError> {
        if t.needs.is_empty() {
            return Ok(vec![]);
        }
        let keys = self.0.store.ticket_keys(&t.needs).await.map_err(internal)?;
        let states = self
            .0
            .store
            .ticket_states(&t.needs)
            .await
            .map_err(internal)?;
        Ok(t.needs
            .iter()
            .filter_map(|n| Some((keys.get(n)?.clone(), *states.get(n)?)))
            .collect())
    }

    async fn unassign(&self, mut t: Ticket, key: &str, by: &By) -> Result<TicketView, DeskError> {
        let Some(was) = t.assignee.take() else {
            return self.view_ticket(t).await;
        };
        t.updated_at = now();
        self.save_ticket(&mut t).await?;
        let text = format!("took {key} off {}", was.name);
        self.ticket_changed(t, by, Kind::TicketAssigned, Some(was), text)
            .await
    }

    pub(super) async fn assign_session(
        &self,
        mut t: Ticket,
        key: &str,
        view: SessionView,
        by: &By,
        tell: bool,
    ) -> Result<TicketView, DeskError> {
        if view.state == SessionState::Closed {
            return Err(DeskError::Conflict(format!(
                "{} has ended; assign {key} to a running session.",
                view.label()
            )));
        }
        let party = Party::session(&view);
        let mut at = now();
        loop {
            t.assignee = Some(party.clone());
            self.to_column(&mut t, TicketState::Doing, at).await?;
            match self.0.store.put_ticket(&mut t).await {
                Ok(()) => break,
                Err(TicketWrite::Stale) => {
                    // Changed meanwhile (a pin, an edit): assigned over
                    // what it is now.
                    t = self.ticket_of(t.id).await?;
                    at = now();
                }
                Err(e) => return Err(stale(e)),
            }
        }
        self.0
            .store
            .put_link(t.id, &Link::Session { session: view.id }, at)
            .await
            .map_err(internal)?;
        if !t.links.contains(&Link::Session { session: view.id }) {
            t.links.push(Link::Session { session: view.id });
        }
        if view.goal.trim().is_empty() {
            self.update(
                view.id,
                SessionPatch {
                    goal: Some(board::goal_of(key, &t)),
                    ..Default::default()
                },
            )?;
        }
        let mut text = format!("gave {key} to {}", view.label());
        if tell {
            let needs = self.need_keys(&t).await?;
            let said = self
                .line_send(
                    sender_of(by),
                    Send::Tell {
                        to: Target::Session(view.id),
                        text: board::handed_text(key, &t, &needs),
                        when: When::Queue,
                    },
                )
                .await;
            if let Err(e) = said {
                tracing::info!(session = %view.id, error = %e, "a ticket's session wasn't told of it");
                text.push_str(&format!("; it wasn't told ({e})"));
            }
        }
        self.ticket_changed(t, by, Kind::TicketAssigned, Some(party), text)
            .await
    }

    async fn assign_agent(
        &self,
        mut t: Ticket,
        key: &str,
        agent: &str,
        by: &By,
    ) -> Result<TicketView, DeskError> {
        let crew = self
            .board_crew()
            .ok_or_else(|| DeskError::Conflict("There are no agents here to give it to.".into()))?;
        let name = crew
            .agents()
            .into_iter()
            .find(|a| a.id == agent)
            .map(|a| a.name)
            .ok_or_else(|| DeskError::Invalid(format!("There's no agent called {agent} here.")))?;
        let party = Party::agent(agent, &name);
        let was = (t.assignee.clone(), t.state, t.rank, t.state_at);
        loop {
            t.assignee = Some(party.clone());
            self.to_column(&mut t, TicketState::Doing, now()).await?;
            match self.0.store.put_ticket(&mut t).await {
                Ok(()) => break,
                Err(TicketWrite::Stale) => t = self.ticket_of(t.id).await?,
                Err(e) => return Err(stale(e)),
            }
        }
        // Saved first: the agent is handed only what the board says is its.
        let needs = self.need_keys(&t).await?;
        if let Err(e) = crew
            .assign_as(agent, &board::handed_text(key, &t, &needs), by)
            .await
        {
            let mut back = t.clone();
            (back.assignee, back.state, back.rank, back.state_at) = was;
            if let Err(undo) = self.0.store.put_ticket(&mut back).await {
                tracing::warn!(ticket = %t.id, error = %undo, "an agent's ticket stays assigned though it wasn't handed");
            }
            return Err(DeskError::Conflict(e));
        }
        let text = format!("gave {key} to {name}");
        self.ticket_changed(t, by, Kind::TicketAssigned, Some(party), text)
            .await
    }

    /// Tells the session `t` is assigned to that it is gone.
    async fn tell_gone(&self, b: &Board, t: &Ticket, by: &By) {
        let Some(session) = t.session() else {
            return;
        };
        let key = t.key(b).unwrap_or_else(|| "A ticket".into());
        // A title the person didn't write never goes in as their words.
        let text = match t.created_by.kind {
            PartyKind::You => format!(
                "{key} “{}” was deleted from the board; it is no longer yours.",
                crew::one_line(&t.title, 120)
            ),
            _ => format!(
                "{key} (added by {}) was deleted from the board; it is no longer yours.",
                t.created_by.name
            ),
        };
        self.tell_session(session, text, by).await;
    }

    /// Tells `session` on its line, from `by`; what can't be sent is
    /// only logged.
    pub(super) async fn tell_session(&self, session: Uuid, text: String, by: &By) {
        let said = self
            .line_send(
                sender_of(by),
                Send::Tell {
                    to: Target::Session(session),
                    text,
                    when: When::Queue,
                },
            )
            .await;
        if let Err(e) = said {
            tracing::info!(%session, error = %e, "a ticket's session wasn't told");
        }
    }

    /// Deletes a ticket: its needs and links go with it, the session
    /// working on it is told, and its history keeps its key in its words.
    pub async fn delete_ticket(&self, id: Uuid, by: &By) -> Result<(), DeskError> {
        let t = self.ticket_of(id).await?;
        let b = self.board_of(t.board).await?;
        self.tell_gone(&b, &t, by).await;
        self.0.store.delete_ticket(id).await.map_err(internal)?;
        let key = t.key(&b).unwrap_or_default();
        self.note(
            by.event(
                Kind::TicketDeleted,
                t.assignee.clone(),
                format!("deleted {key} “{}”", crew::one_line(&t.title, 120)),
            )
            .about_ticket(id, b.group.as_deref()),
        );
        self.announce(DeskEvent::TicketRemoved { id });
        Ok(())
    }

    /// A session is being forgotten: the tickets assigned to it are no
    /// longer anyone's. (The store unassigns them with the session.)
    pub(super) async fn board_forgotten(&self, gone: Uuid) {
        let tickets = match self.0.store.session_tickets(gone).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(session = %gone, error = format!("{e:#}"), "couldn't read a forgotten session's tickets");
                return;
            }
        };
        for mut t in tickets {
            t.assignee = None;
            t.links.retain(|l| *l != Link::Session { session: gone });
            if let Ok(v) = self.view_ticket(t).await {
                self.announce(DeskEvent::Ticket {
                    ticket: Box::new(v),
                });
            }
        }
    }

    // ── Pull requests ────────────────────────────────────────────────────

    /// The ticket a session's pull request is for: the one assigned to it,
    /// else one linked to it, else one linked to a session on the same
    /// branch; the latest changed that hasn't settled.
    async fn ticket_for_pr(&self, session: Uuid) -> Result<Option<Ticket>, DeskError> {
        let at = now();
        let pick = |mut ts: Vec<Ticket>| {
            ts.retain(|t| !board::settled(t, at));
            ts.into_iter().max_by_key(|t| t.updated_at)
        };
        let store = &self.0.store;
        if let Some(t) = pick(store.session_tickets(session).await.map_err(internal)?) {
            return Ok(Some(t));
        }
        if let Some(t) = pick(store.linked_tickets(&[session]).await.map_err(internal)?) {
            return Ok(Some(t));
        }
        let me = self.view(session)?;
        let Some(branch) = me.branch.as_deref() else {
            return Ok(None);
        };
        let twins: Vec<Uuid> = self
            .list()
            .into_iter()
            .filter(|s| s.id != session && s.branch.as_deref() == Some(branch))
            .map(|s| s.id)
            .collect();
        if twins.is_empty() {
            return Ok(None);
        }
        Ok(pick(store.linked_tickets(&twins).await.map_err(internal)?))
    }

    /// A session's pull request moved on: its ticket links it and follows
    /// it (opened → Review, merged → Done), recorded as Desk's doing. What
    /// watches pull requests calls this; `None` when the session has no
    /// ticket.
    pub async fn ticket_pr(&self, session: Uuid, pr: Pr) -> Result<Option<TicketView>, DeskError> {
        let Some(mut t) = self.ticket_for_pr(session).await? else {
            return Ok(None);
        };
        let link = Link::Pr {
            number: pr.number,
            url: pr.url.clone(),
        };
        let at = now();
        self.0
            .store
            .put_link(t.id, &link, at)
            .await
            .map_err(internal)?;
        if !t.links.contains(&link) {
            t.links.push(link);
        }
        let desk = By {
            party: Party::desk(),
            reason: None,
            suggestion_id: None,
            hop: 0,
            preference: None,
        };
        let why = format!("pull request #{} {}", pr.number, pr.state.as_str());
        match board::follows_pr(t.state, pr.state) {
            Some(to) => {
                let from = t.state;
                self.to_column(&mut t, to, at).await?;
                self.save_ticket(&mut t).await?;
                self.ticket_moved(t, from, &desk, Some(&why))
                    .await
                    .map(Some)
            }
            None => {
                let key = self.key_of(&t).await?;
                let text = format!("linked {why} to {key}");
                self.ticket_changed(t, &desk, Kind::TicketEdited, None, text)
                    .await
                    .map(Some)
            }
        }
    }

    // ── The `ticket` tool on a line ──────────────────────────────────────

    /// The ticket a session names by `key`, or its own when it names none.
    async fn named_ticket(&self, caller: Uuid, key: Option<&str>) -> Result<Ticket, String> {
        let fail = |e: DeskError| e.to_string();
        match key {
            Some(key) => {
                if let Some(t) = self
                    .synced_by_key(key, |b| self.reaches_board(caller, b))
                    .await
                    .map_err(fail)?
                {
                    return Ok(t);
                }
                let (prefix, n) = board::parse_key(key)
                    .ok_or_else(|| format!("{key:?} isn't a ticket's key, like ACM-12."))?;
                let b = self
                    .0
                    .store
                    .board_by_prefix(&prefix)
                    .await
                    .map_err(|e| fail(internal(e)))?
                    .filter(|b| self.reaches_board(caller, b))
                    .ok_or_else(|| format!("No board you can reach has keys starting {prefix}."))?;
                self.0
                    .store
                    .ticket_by_number(b.id, n)
                    .await
                    .map_err(|e| fail(internal(e)))?
                    .ok_or_else(|| format!("There's no {prefix}-{n}."))
            }
            None => {
                let at = now();
                let mut mine = self
                    .0
                    .store
                    .session_tickets(caller)
                    .await
                    .map_err(|e| fail(internal(e)))?;
                mine.retain(|t| !board::settled(t, at));
                match mine.len() {
                    0 => Err("No ticket is assigned to you; say its key.".into()),
                    1 => Ok(mine.remove(0)),
                    _ => Err("More than one ticket is yours; say which by its key.".into()),
                }
            }
        }
    }

    /// Whether the session `caller` may read and write tickets on `b`.
    fn reaches_board(&self, caller: Uuid, b: &Board) -> bool {
        let group = self.view(caller).ok().and_then(|v| v.group.map(|g| g.id));
        board::board_reachable(
            self.line_reach(),
            group.as_deref(),
            b.group.as_deref(),
            &self.bossed(),
        )
    }

    /// What a session reads of a ticket.
    async fn ticket_said(&self, caller: Uuid, t: Ticket) -> Result<Value, DeskError> {
        let v = self.view_ticket(t).await?;
        // What it needs, as far as the session can see: a need on a board
        // out of its reach is counted, never named.
        let mut needs = vec![];
        let mut blocked = vec![];
        let mut hidden = 0;
        for id in &v.ticket.needs {
            let Some(n) = self.0.store.ticket(*id).await.map_err(internal)? else {
                continue;
            };
            let b = self.board_of(n.board).await?;
            if !self.reaches_board(caller, &b) {
                hidden += 1;
                continue;
            }
            let key = n.key(&b).unwrap_or_else(|| "a ticket".into());
            if v.blockers.contains(id) {
                blocked.push(key.clone());
            }
            needs.push(json!({ "key": key, "state": n.state }));
        }
        let outside = match hidden {
            0 => None,
            1 => Some("It also needs 1 other ticket outside your reach.".to_string()),
            n => Some(format!(
                "It also needs {n} other tickets outside your reach."
            )),
        };
        let by = &v.ticket.created_by;
        let note = (by.kind != PartyKind::You).then(|| {
            format!(
                "{} added this ticket; its title and description are that {}'s note, not the person's words.",
                by.name,
                match by.kind {
                    PartyKind::Session => "session",
                    _ => "party",
                }
            )
        });
        // A synced ticket's words are its issue's: fenced as untrusted.
        let (title, body, note) = match v.ticket.from_remote() {
            Some(system) => {
                let key = v.key.clone().unwrap_or_default();
                let nonce = crate::board_sync::nonce();
                (
                    crate::board_sync::fence(system, &key, &v.ticket.title, &nonce),
                    crate::board_sync::fence(system, &key, &v.ticket.body, &nonce),
                    Some(format!(
                        "{key} is a {} issue: its title and description are its writers' words, not the person's.",
                        system.title()
                    )),
                )
            }
            None => (v.ticket.title.clone(), v.ticket.body.clone(), note),
        };
        Ok(json!({
            "key": v.key,
            "title": title,
            "body": body,
            "state": v.ticket.state,
            "createdBy": by.name,
            "note": note,
            "assignee": v.ticket.assignee.as_ref().map(|a| a.name.clone()),
            "needs": needs,
            "blockedBy": blocked,
            "outsideReach": outside,
            "links": v.ticket.links,
        }))
    }

    /// The board a session's new ticket goes on: the one `prefix` names,
    /// else its own ticket's, else the only board there is.
    async fn board_for_new(&self, caller: Uuid, prefix: Option<&str>) -> Result<Board, String> {
        let fail = |e: eyre::Report| internal(e).to_string();
        if let Some(p) = prefix {
            let p = p.trim().to_ascii_uppercase();
            let p = p.split('-').next().unwrap_or_default().to_string();
            return self
                .0
                .store
                .board_by_prefix(&p)
                .await
                .map_err(fail)?
                .filter(|b| self.reaches_board(caller, b))
                .ok_or_else(|| format!("No board you can reach has keys starting {p}."));
        }
        let mine = self.0.store.session_tickets(caller).await.map_err(fail)?;
        if let Some(t) = mine.iter().max_by_key(|t| t.updated_at) {
            if let Ok(b) = self.board_of(t.board).await {
                return Ok(b);
            }
        }
        let mut boards = self.0.store.boards().await.map_err(fail)?;
        boards.retain(|b| self.reaches_board(caller, b));
        match boards.len() {
            0 => Err("There's no board here you can reach; ask the person to make one.".into()),
            1 => Ok(boards.remove(0)),
            _ => {
                let prefixes: Vec<String> = boards.into_iter().filter_map(|b| b.prefix).collect();
                Err(format!(
                    "Say which board by its prefix in key: {}.",
                    prefixes.join(", ")
                ))
            }
        }
    }

    /// Runs the line's `ticket` tool for the session `caller`.
    pub(super) async fn ticket_call(
        &self,
        caller: Uuid,
        call: TicketCall,
    ) -> Result<Value, String> {
        let me = self
            .view(caller)
            .map_err(|_| "Desk doesn't know this session.".to_string())?;
        let by = By {
            party: Party::session(&me),
            reason: None,
            suggestion_id: None,
            hop: 0,
            preference: None,
        };
        let say = |e: DeskError| match e {
            DeskError::NotFound => "That ticket is gone.".to_string(),
            DeskError::Internal(e) => {
                tracing::warn!(session = %caller, error = format!("{e:#}"), "the ticket tool failed");
                "Desk can't be reached right now; carry on without it.".into()
            }
            e => e.to_string(),
        };
        match call {
            TicketCall::Mine => {
                let at = now();
                let mut mine = self
                    .0
                    .store
                    .session_tickets(caller)
                    .await
                    .map_err(|e| say(internal(e)))?;
                mine.retain(|t| !board::settled(t, at));
                let mut out = vec![];
                for t in mine {
                    out.push(self.ticket_said(caller, t).await.map_err(say)?);
                }
                Ok(json!({ "tickets": out }))
            }
            TicketCall::Get { key } => {
                let t = self.named_ticket(caller, Some(&key)).await?;
                self.ticket_said(caller, t).await.map_err(say)
            }
            TicketCall::Update { key, state } => {
                let t = self.named_ticket(caller, key.as_deref()).await?;
                let k = self.key_of(&t).await.map_err(say)?;
                if t.session() != Some(caller) {
                    return Err(match &t.assignee {
                        Some(a) => format!("{k} isn't yours: it's {}'s.", a.name),
                        None => format!("{k} isn't yours: it isn't assigned to anyone."),
                    });
                }
                board::session_may_move(t.state, state)?;
                // Moved as read: had the person taken it off this session
                // meanwhile, the write is refused.
                let v = self
                    .move_read(
                        t,
                        Move {
                            state,
                            before: None,
                            after: None,
                        },
                        &by,
                    )
                    .await
                    .map_err(say)?;
                Ok(json!({ "key": v.key, "state": v.ticket.state }))
            }
            TicketCall::Comment { key, text } => {
                let t = self.named_ticket(caller, key.as_deref()).await?;
                let k = self.key_of(&t).await.map_err(say)?;
                let text = crate::line::typable(&text).trim().to_string();
                board::within("comment", &text, board::COMMENT_MAX)?;
                let group = self.board_of(t.board).await.ok().and_then(|b| b.group);
                self.note(
                    by.event(
                        Kind::TicketCommented,
                        None,
                        format!("commented on {k}: “{}”", crew::clip(&text, 400)),
                    )
                    .detail(json!({ "comment": text }))
                    .about_ticket(t.id, group.as_deref()),
                );
                Ok(json!({ "key": k, "commented": true }))
            }
            TicketCall::Create {
                board: prefix,
                text,
            } => {
                let (title, body) = board::title_and_body(&crate::line::typable(&text));
                board::within("title", &title, board::TITLE_MAX)?;
                board::within("description", &body, board::BODY_MAX)?;
                let made = self
                    .0
                    .store
                    .tickets_made_by(caller, now() - chrono::Duration::hours(1))
                    .await
                    .map_err(|e| say(internal(e)))?;
                if made >= board::CREATES_PER_HOUR {
                    return Err(format!(
                        "You have added {made} tickets in the last hour, the most a session may; tell the person what else needs doing instead."
                    ));
                }
                let b = self.board_for_new(caller, prefix.as_deref()).await?;
                // A session's words never become an issue on someone's Jira.
                if let Some(sync) = &b.sync {
                    return Err(format!(
                        "{} is synced with {} {}; tell the person what needs adding there.",
                        b.name,
                        sync.system.title(),
                        sync.target
                    ));
                }
                let v = self
                    .create_ticket(
                        NewTicket {
                            board: b.id,
                            title,
                            body,
                            state: Some(TicketState::Backlog),
                            needs: vec![],
                        },
                        &by,
                    )
                    .await
                    .map_err(say)?;
                Ok(json!({ "key": v.key, "state": v.ticket.state }))
            }
        }
    }
}

/// Who sends a ticket's message on a line, for `by`.
fn sender_of(by: &By) -> Sender {
    match by.party.kind {
        PartyKind::Manager => Sender::Overseer(by.clone()),
        PartyKind::Session => match by.party.id.parse() {
            Ok(id) => Sender::Session(id),
            Err(_) => Sender::Person,
        },
        PartyKind::You | PartyKind::Agent | PartyKind::Desk => Sender::Person,
    }
}

#[cfg(test)]
#[path = "board_tests.rs"]
mod tests;
