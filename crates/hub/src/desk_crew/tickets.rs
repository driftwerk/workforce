//! The boss and the tickets on Desk's own boards: `crew.ticket` reads one
//! in full, and `crew.edit_ticket`, `crew.comment_ticket`,
//! `crew.link_ticket`, `crew.create_ticket` and `crew.delete_ticket` change
//! them, beside `crew.move_ticket` and `crew.assign_ticket`
//! ([`super::actions`]). A board synced with Jira, Linear or GitHub is
//! refused here: its issues change through their own tools.
//!
//! Every change goes through the same Desk calls the person's board makes,
//! so the board updates live and each ticket's history says who did it. A
//! change runs at Act and Act fully as ordinary work and is a suggestion at
//! Suggest; deleting a ticket can't be undone, so it is always the person's
//! yes, at every level.
//!
//! A ticket of a plan that is still open takes its words and where it runs
//! from the plan: `crew.edit_ticket` changes them through the plan
//! ([`super::plan`]'s `tickets_edit`), under the plan's own rules (a new
//! brief waits for the person, a move the approved plan already allows
//! applies at once). A ticket in no plan keeps no machine, folder, engine
//! or model of its own: they are said as it is handed out.

use futures::future::BoxFuture;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::board::{Board, TicketState, BODY_MAX, TITLE_MAX};
use workforce_desk::crew::{self, By, PartyKind};
use workforce_desk::scope::Seen;
use workforce_desk::{CommentPost, NewTicket, TicketPatch, TicketView};

use super::actions::Action;
use super::tools::{cell, Bound};
use super::watcher::reported;
use super::CrewHub;

pub const READ: &str = "crew.ticket";
pub const EDIT: &str = "crew.edit_ticket";
pub const COMMENT: &str = "crew.comment_ticket";
pub const LINK: &str = "crew.link_ticket";
pub const CREATE: &str = "crew.create_ticket";
pub const DELETE: &str = "crew.delete_ticket";

pub const READ_DESCRIPTION: &str = "One ticket in full (ticket, its key like ACM-12): its title, \
brief, column, who has it, what it needs and what waits on it, its comments and its history, and, \
for a ticket of a plan, the plan and where it runs (machine, folder, engine, model). Read it before \
you change a ticket.";

pub const EDIT_DESCRIPTION: &str =
    "Change a ticket on one of Desk's own boards (ticket, its key): \
its title, its brief in full (body), or words added to the end of its brief (append, which keeps \
what it says). Only what you give changes. For a ticket of a plan that hasn't started, machine, \
folder, engine and model say where its session runs: that, and its words, change through the plan, \
under its rules (before the person approves the plan a change applies at once; after, a new brief \
waits for their yes, and a ticket that has started can't move). \
A ticket in no plan says where it runs as it is handed out (crew.assign_ticket). To record \
something on a ticket (an approval, a decision, a finding), comment on it instead: \
crew.comment_ticket.";

pub const COMMENT_DESCRIPTION: &str = "Add a comment to a ticket on one of Desk's own boards \
(ticket, its key; comment, its words): how to record on it what was decided or approved, what was \
found, or where it stands. It shows on the ticket as yours.";

pub const LINK_DESCRIPTION: &str =
    "Make a ticket on one of Desk's own boards need another (ticket, \
its key; needs, the other's key): it isn't ready until that one is done. unlink: true takes the \
need off.";

pub const CREATE_DESCRIPTION: &str = "Make a ticket on one of Desk's own boards (board, its name or \
prefix; your only one when you have one): title, body (its brief), state (backlog or todo; todo when \
not said) and needs (keys of tickets it needs done first). For work of a plan, use crew.plan.";

pub const DELETE_DESCRIPTION: &str =
    "Delete a ticket on one of Desk's own boards for good (ticket, \
its key). It can't be undone, so it is always the person's yes: it becomes a card they accept or \
dismiss. To set one aside, move it to dropped instead (crew.move_ticket).";

/// Why a delete is always the person's yes.
pub(crate) const DELETE_WHY: &str =
    "deleting a ticket can't be undone, so it's the person's yes: they delete it with one click";

/// How many of a ticket's comments and history events a read gives.
const SHOWN: usize = 20;
/// The most characters of each comment a read gives (a brief is given
/// whole: it is at most `BODY_MAX`).
const COMMENT_CHARS: usize = 2_000;

fn ticket_schema() -> Value {
    json!({ "type": "string", "description": "A ticket's key, like ACM-12" })
}

pub(crate) fn read_schema() -> Value {
    json!({ "type": "object", "properties": { "ticket": ticket_schema() }, "required": ["ticket"] })
}

/// The properties of an acting tool's input, beside its reason.
pub(crate) fn properties(name: &str) -> (Value, &'static [&'static str]) {
    match name {
        EDIT => (
            json!({
                "ticket": ticket_schema(),
                "title": { "type": "string", "description": "Its new title" },
                "body": { "type": "string", "description": "Its new brief, in full: replaces what it says" },
                "append": { "type": "string", "description": "Words added to the end of its brief, after a blank line; what it says is kept" },
                "machine": { "type": "string", "description": "A plan's ticket only: the person's machine it runs on, as the sidebar names it; \"this computer\" for this one" },
                "folder": { "type": "string", "description": "A plan's ticket only: the absolute folder it works in, on the machine it runs on" },
                "engine": { "type": "string", "description": "A plan's ticket only, and only when the person named one: claude or codex" },
                "model": { "type": "string", "description": "A plan's ticket only, and only when the person named one: as its engine names it; \"\" is the engine's own" }
            }),
            &["ticket"],
        ),
        COMMENT => (
            json!({
                "ticket": ticket_schema(),
                "comment": { "type": "string", "description": "What the comment says" }
            }),
            &["ticket", "comment"],
        ),
        LINK => (
            json!({
                "ticket": ticket_schema(),
                "needs": { "type": "string", "description": "The key of the ticket it needs done first" },
                "unlink": { "type": "boolean", "description": "true takes that need off" }
            }),
            &["ticket", "needs"],
        ),
        CREATE => (
            json!({
                "board": { "type": "string", "description": "One of your boards, by its name or prefix" },
                "title": { "type": "string" },
                "body": { "type": "string", "description": "Its brief" },
                "state": { "type": "string", "enum": ["backlog", "todo"], "description": "todo when not said" },
                "needs": { "type": "array", "items": { "type": "string" }, "description": "Keys of tickets it needs done first" }
            }),
            &["title"],
        ),
        _ => (json!({ "ticket": ticket_schema() }), &["ticket"]),
    }
}

/// What a boss changes of a ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TicketChange {
    /// Its words: a title, a whole brief, or words added to its brief.
    Edit {
        title: Option<String>,
        body: Option<String>,
        append: Option<String>,
        /// A brief in full replaces the one there when it was suggested:
        /// that one's digest, kept with the suggestion, so one changed
        /// since is never written over.
        was: Option<String>,
    },
    Comment {
        text: String,
    },
    /// That it needs `other` (by key; its id once resolved), or no longer.
    Need {
        other: String,
        other_id: Option<Uuid>,
        needs: bool,
    },
    Delete,
}

/// A string field of a tool's input: trimmed, `None` when absent or blank.
fn said(input: &Value, key: &str) -> Result<Option<String>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Some(_) => Err(format!("{key} is a string.")),
    }
}

/// The fields of `crew.edit_ticket` that say where a ticket runs.
pub(crate) const RUNS: [&str; 4] = ["machine", "folder", "engine", "model"];

fn longest(what: &str, text: &str, max: usize) -> Result<(), String> {
    let n = text.chars().count();
    match n > max {
        true => Err(format!("Its {what} is {n} characters; keep it to {max}.")),
        false => Ok(()),
    }
}

/// The change a ticket tool's input asks for.
pub(crate) fn change_of(tool: &str, input: &Value) -> Result<TicketChange, String> {
    Ok(match tool {
        EDIT => {
            if RUNS
                .iter()
                .any(|f| input.get(*f).is_some_and(|v| !v.is_null()))
            {
                // A plan's ticket is changed through its plan, before this
                // is read ([`CrewHub::edit_by_plan`]).
                return Err("Only a ticket of an open plan says where it runs (machine, folder, engine, model): say those when you hand it out, with crew.assign_ticket.".into());
            }
            let title = said(input, "title")?;
            // A brief in full may be emptied: `""` is said.
            let body = match input.get("body") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.trim().to_string()),
                Some(_) => return Err("body is a string.".into()),
            };
            let append = said(input, "append")?;
            if body.is_some() && append.is_some() {
                return Err(
                    "Give its brief in full (body) or words to add to it (append), not both."
                        .into(),
                );
            }
            if title.is_none() && body.is_none() && append.is_none() {
                return Err("Say what changes: title, body or append.".into());
            }
            if let Some(t) = &title {
                longest("title", t, TITLE_MAX)?;
            }
            if let Some(b) = body.as_ref().or(append.as_ref()) {
                longest("brief", b, BODY_MAX)?;
            }
            TicketChange::Edit {
                title,
                body,
                append,
                // Only ever the digest Desk kept, with a suggestion.
                was: said(input, "brief_was")?,
            }
        }
        COMMENT => {
            let text = said(input, "comment")?.ok_or("Say the comment.")?;
            longest("comment", &text, workforce_desk::ticket_comments::BODY_MAX)?;
            TicketChange::Comment { text }
        }
        LINK => {
            let other = said(input, "needs")?
                .ok_or("Say which ticket it needs, by its key (like ACM-12).")?;
            let other = workforce_desk::board::parse_key(&other)
                .map(|(prefix, n)| format!("{prefix}-{n}"))
                .ok_or_else(|| format!("{other:?} isn’t a ticket’s key, like ACM-12."))?;
            TicketChange::Need {
                other,
                // Only ever the id Desk resolved, kept with a suggestion.
                other_id: said(input, "needs_id")?
                    .map(|i| i.parse().map_err(|_| "That isn't a ticket id.".to_string()))
                    .transpose()?,
                needs: !input["unlink"].as_bool().unwrap_or(false),
            }
        }
        DELETE => TicketChange::Delete,
        other => return Err(format!("{other} is not something the overseer can do")),
    })
}

/// A new ticket, as `crew.create_ticket` asks for it: its board as named
/// (its id once resolved), title, brief, column, and needs by key (their
/// ids once resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewOne {
    pub board: Option<String>,
    pub board_id: Option<Uuid>,
    pub title: String,
    pub body: String,
    pub state: TicketState,
    pub needs: Vec<String>,
    pub need_ids: Vec<Uuid>,
}

pub(crate) fn new_of(input: &Value) -> Result<NewOne, String> {
    let title = said(input, "title")?.ok_or("Say its title.")?;
    longest("title", &title, TITLE_MAX)?;
    let body = said(input, "body")?.unwrap_or_default();
    longest("brief", &body, BODY_MAX)?;
    let state = match said(input, "state")?.as_deref() {
        None | Some("todo") => TicketState::Todo,
        Some("backlog") => TicketState::Backlog,
        Some(_) => return Err("A new ticket starts in backlog or todo; move it on after.".into()),
    };
    let needs = match input.get("needs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(keys)) => keys
            .iter()
            .map(|k| {
                let k = k
                    .as_str()
                    .map(str::trim)
                    .ok_or("needs is a list of tickets' keys.")?;
                workforce_desk::board::parse_key(k)
                    .map(|(prefix, n)| format!("{prefix}-{n}"))
                    .ok_or_else(|| format!("{k:?} isn’t a ticket’s key, like ACM-12."))
            })
            .collect::<Result<_, String>>()?,
        Some(_) => return Err("needs is a list of tickets' keys.".into()),
    };
    // Only ever the id Desk resolved, kept with a suggestion.
    let board_id = said(input, "board_id")?
        .map(|b| b.parse().map_err(|_| "That isn't a board id.".to_string()))
        .transpose()?;
    let need_ids = match input.get("need_ids") {
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|i| {
                i.as_str()
                    .and_then(|i| i.parse().ok())
                    .ok_or_else(|| "That isn't a ticket id.".to_string())
            })
            .collect::<Result<Vec<Uuid>, String>>()?,
        _ => Vec::new(),
    };
    Ok(NewOne {
        board: said(input, "board")?,
        board_id,
        title,
        body,
        state,
        needs,
        need_ids,
    })
}

/// A change in the words a suggestion card says.
pub(crate) fn describe(key: &str, change: &TicketChange) -> String {
    match change {
        TicketChange::Edit {
            title,
            body,
            append,
            ..
        } => {
            let mut parts = Vec::new();
            if let Some(t) = title {
                parts.push(format!("rename it “{}”", crew::clip(t, 120)));
            }
            if let Some(b) = body {
                parts.push(format!("replace its brief with “{}”", crew::clip(b, 200)));
            }
            if let Some(a) = append {
                parts.push(format!("add to its brief: “{}”", crew::clip(a, 200)));
            }
            format!("On {key}, {}", parts.join(" and "))
        }
        TicketChange::Comment { text } => {
            format!("Comment on {key}: “{}”", crew::clip(text, 200))
        }
        TicketChange::Need {
            other, needs: true, ..
        } => format!("Make {key} need {other}"),
        TicketChange::Need { other, .. } => format!("Make {key} no longer need {other}"),
        TicketChange::Delete => format!("Delete {key} for good (it can't be undone)"),
    }
}

/// Who has a ticket, as a read says it.
fn holder(v: &TicketView) -> Value {
    match &v.ticket.assignee {
        None => Value::Null,
        Some(a) => json!({
            "name": a.name,
            "kind": match a.kind {
                PartyKind::Session => "session",
                PartyKind::Agent => "agent",
                PartyKind::You => "the person",
                PartyKind::Manager => "a boss",
                PartyKind::Desk => "Desk",
            },
            "id": a.id,
        }),
    }
}

fn column(s: TicketState) -> &'static str {
    match s {
        TicketState::Backlog => "backlog",
        TicketState::Todo => "todo",
        TicketState::Doing => "doing",
        TicketState::Review => "review",
        TicketState::Done => "done",
        TicketState::Dropped => "dropped",
    }
}

impl CrewHub {
    /// The board `t` is on, when it is one of Desk's own: one synced with
    /// Jira, Linear or GitHub (or a ticket of its issue) is refused, naming
    /// where it changes.
    pub(crate) async fn own_board_of(&self, t: &TicketView) -> Result<Board, String> {
        let key = t.key.as_deref().unwrap_or("That ticket");
        let b = self
            .desk
            .store()
            .board(t.ticket.board)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| format!("{key}'s board isn't there any more."))?;
        if let Some(system) = t.ticket.from_remote() {
            return Err(format!(
                "{key} is a {} issue: change it there, with the {} tools.",
                system.title(),
                system.as_str()
            ));
        }
        if let Some(s) = &b.sync {
            return Err(format!(
                "{key} is on {}, a board synced with {}: change its tickets there, with the {} tools.",
                b.name,
                s.system.title(),
                s.system.as_str()
            ));
        }
        Ok(b)
    }

    /// The board a new ticket goes on: the one named (by name or prefix)
    /// among Desk's own that `seen` sees, or the only such one.
    pub(crate) async fn own_board_named(
        &self,
        named: Option<&str>,
        id: Option<Uuid>,
        seen: Option<&Seen>,
    ) -> Result<Board, String> {
        let boards = self.desk.boards().await.map_err(|e| {
            tracing::warn!(error = %e, "reading the boards for a new ticket failed");
            "The boards could not be read just now.".to_string()
        })?;
        let mine: Vec<Board> = boards
            .into_iter()
            .filter(|b| seen.is_none_or(|s| s.sees(b.group.as_deref())))
            .collect();
        if let Some(id) = id {
            return mine
                .into_iter()
                .find(|b| b.id == id)
                .ok_or_else(|| "That board isn't there any more.".to_string())
                .and_then(|b| match &b.sync {
                    Some(s) => Err(format!(
                        "{} is synced with {}: make its tickets there.",
                        b.name,
                        s.system.title()
                    )),
                    None => Ok(b),
                });
        }
        let found: Vec<&Board> = match named {
            Some(n) => mine
                .iter()
                .filter(|b| {
                    b.name.eq_ignore_ascii_case(n)
                        || b.prefix
                            .as_deref()
                            .is_some_and(|p| p.eq_ignore_ascii_case(n))
                })
                .collect(),
            None => mine.iter().filter(|b| b.sync.is_none()).collect(),
        };
        match found.as_slice() {
            [b] if b.sync.is_some() => Err(format!(
                "{} is synced with {}: make its tickets there, with its own tools.",
                b.name,
                b.sync
                    .as_ref()
                    .map_or("another tracker", |s| s.system.title())
            )),
            [b] => Ok((*b).clone()),
            [] => Err(match named {
                Some(n) => format!("You have no board called {n:?}."),
                None => "You have no board of Desk's own: make one with crew.board_create.".into(),
            }),
            _ => Err(format!(
                "Say which board: {}.",
                found
                    .iter()
                    .map(|b| match &b.prefix {
                        Some(p) => format!("{} ({p})", b.name),
                        None => b.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// `crew.ticket`: one ticket in full, of the boards `b`'s scope sees.
    /// Its words, and its comments', are someone's own: given as reported.
    pub(super) async fn ticket_tool(&self, b: &Bound, input: &Value) -> Result<Value, String> {
        let key = super::actions::ticket_key(input["ticket"].as_str().map(str::to_string))?;
        let t = self.ticket_named(&key, None, Some(&b.seen)).await?;
        let key = t.key.clone().unwrap_or(key);
        let id = t.ticket.id;
        let board = self.desk.store().board(t.ticket.board).await.ok().flatten();
        // Only the needs on boards this scope sees are named.
        let mut seen_needs = Vec::new();
        for n in &t.ticket.needs {
            let group = match self.desk.ticket(*n).await {
                Ok(o) => self.desk.group_of_board(o.ticket.board).await.ok(),
                Err(_) => None,
            };
            if group.is_some_and(|g| b.seen.sees(g.as_deref())) {
                seen_needs.push(*n);
            }
        }
        let names = match seen_needs.is_empty() {
            true => std::collections::HashMap::new(),
            false => self
                .desk
                .store()
                .ticket_keys(&seen_needs)
                .await
                .unwrap_or_default(),
        };
        let needs: Vec<Value> = t
            .ticket
            .needs
            .iter()
            .map(|n| match names.get(n) {
                Some(k) => json!({ "ticket": k, "done": !t.blockers.contains(n) }),
                None => json!({ "ticket": "a ticket elsewhere" }),
            })
            .collect();
        let brief = &t.ticket.body;
        let mut out = json!({
            "ticket": key,
            "board": board.as_ref().map(|b| b.name.clone()),
            "synced": t.ticket.from_remote().map(|s| s.title()),
            "title": reported(&t.ticket.title, &format!("{key}'s title")),
            "brief": reported(brief, &format!("{key}'s brief")),
            "column": column(t.ticket.state),
            "ready": t.ready,
            "has_it": holder(&t),
            "needs": needs,
            "pinned": t.ticket.pinned,
            "snoozed": t.snoozed,
            "created_by": t.ticket.created_by.name,
            "created_at": t.ticket.created_at.to_rfc3339(),
            "updated_at": t.ticket.updated_at.to_rfc3339(),
        });
        // Its plan, and where it runs there.
        if let Ok(Some(plan)) = self.desk.store().plan_of_ticket(id).await {
            if let Ok(Some(p)) = self.desk.store().plan(plan).await {
                let runs = self
                    .desk
                    .store()
                    .plan_runs(plan)
                    .await
                    .ok()
                    .and_then(|r| r.into_iter().find(|(t, _)| *t == id))
                    .map(|(_, r)| r);
                // A brief the plan holds, still waiting for the person.
                let pending = match self.desk.plan(plan).await {
                    Ok(d) => d
                        .revisions
                        .last()
                        .and_then(|r| r.snapshot.ticket(id))
                        .map(|n| n.body.clone())
                        .filter(|b| *b != t.ticket.body),
                    Err(_) => None,
                };
                out["plan"] = json!({
                    "brief_waiting": pending.map(|b| reported(&b, &format!("{key}'s brief, as its plan's newest revision says it"))),
                    "plan_id": p.id.to_string(),
                    "goal": reported(&p.goal, "a plan's goal"),
                    "state": p.state.as_str(),
                    "yours": p.boss == b.service.agent_id.as_str(),
                    "taken_on": runs.as_ref().is_some_and(|r| r.adopted),
                    "machine": runs.as_ref().map(|r| r.machine.clone().unwrap_or_else(|| "this computer".into())),
                    "folder": runs.as_ref().and_then(|r| r.folder.clone()),
                    "engine": runs.as_ref().and_then(|r| r.engine.map(|e| e.as_str())),
                    "model": runs.as_ref().and_then(|r| r.model.clone()),
                });
            }
        }
        match self.desk.ticket_comments(id).await {
            Ok(all) => {
                let n = all.len();
                let shown: Vec<Value> = all
                    .iter()
                    .skip(n.saturating_sub(SHOWN))
                    .map(|c| {
                        let body: String = c.body.chars().take(COMMENT_CHARS).collect();
                        json!({
                            "by": format!("{} ({})", c.author.name, c.author.on),
                            "at": c.at.to_rfc3339(),
                            "says": reported(&body, &format!("a comment on {key}")),
                        })
                    })
                    .collect();
                out["comments"] = json!(shown);
                if n > SHOWN {
                    out["comments_note"] = json!(format!("The last {SHOWN} of its {n} comments."));
                }
            }
            Err(e) => {
                tracing::warn!(ticket = %id, error = %e, "a ticket's comments couldn't be read for crew.ticket");
                out["comments_note"] = json!("Its comments couldn't be read just now.");
            }
        }
        match self.desk.ticket_history(id, None, SHOWN as i64).await {
            Ok(page) => {
                // Its lines carry its title and others' names as typed.
                let lines: Vec<String> = page
                    .events
                    .iter()
                    .map(|e| {
                        format!(
                            "{} · {} · {}",
                            e.at.to_rfc3339(),
                            e.actor.name,
                            cell(&e.text)
                        )
                    })
                    .collect();
                out["history"] = json!(reported(&lines.join("\n"), &format!("{key}'s history")));
            }
            Err(e) => {
                tracing::warn!(ticket = %id, error = %e, "a ticket's history couldn't be read for crew.ticket");
            }
        }
        Ok(out)
    }

    /// `crew.edit_ticket` for a ticket of an open plan: its words and where
    /// it runs change through the plan, as `crew.plan_update` changes them
    /// by key. `None` when the change is the board's to make: a ticket in no
    /// open plan, or the words of one a plan took on from its board.
    pub(super) async fn edit_by_plan(
        &self,
        b: &Bound,
        input: &Value,
    ) -> Result<Option<Value>, String> {
        let key = super::actions::ticket_key(input["ticket"].as_str().map(str::to_string))?;
        let t = self.ticket_named(&key, None, Some(&b.seen)).await?;
        let key = t.key.clone().unwrap_or(key);
        self.own_board_of(&t).await?;
        let runs = RUNS
            .iter()
            .any(|f| input.get(*f).is_some_and(|v| !v.is_null()));
        // A blank title or addition says nothing; an empty brief is said.
        let words = said(input, "title")?.is_some()
            || said(input, "append")?.is_some()
            || input.get("body").is_some_and(|v| !v.is_null());
        let store = self.desk.store();
        let plan = match store.plan_of_ticket(t.ticket.id).await {
            Ok(Some(id)) => store.plan(id).await.ok().flatten(),
            _ => None,
        };
        let open = plan.filter(|p| {
            !matches!(
                p.state,
                workforce_desk::plan::PlanState::Done | workforce_desk::plan::PlanState::Dropped
            )
        });
        let Some(p) = open else {
            // Read as the board's change, which refuses where it runs.
            return Ok(None);
        };
        let adopted = store
            .plan_runs(p.id)
            .await
            .ok()
            .and_then(|r| r.into_iter().find(|(id, _)| *id == t.ticket.id))
            .is_some_and(|(_, r)| r.adopted);
        // What a ticket taken on says is its board's.
        if adopted && !runs {
            return Ok(None);
        }
        if adopted && words {
            return Err(format!(
                "{key} was taken on by a plan from its board: change where it runs (machine, folder, engine, model) in one call, and its words in another."
            ));
        }
        if p.boss != b.service.agent_id.as_str() {
            return Err(format!(
                "{key} is a ticket of a plan another boss runs: its brief and where it runs change only through that plan. Comment on it instead (crew.comment_ticket), or tell the person."
            ));
        }
        let detail = self.desk.plan(p.id).await.map_err(|e| {
            tracing::warn!(plan = %p.id, error = %e, "reading a ticket's plan failed");
            "Its plan couldn't be read just now.".to_string()
        })?;
        let mut edit = serde_json::Map::new();
        match &t.key {
            Some(k) => edit.insert("key".into(), json!(k)),
            None => edit.insert("id".into(), json!(t.ticket.id.to_string())),
        };
        if let Some(title) = said(input, "title")? {
            longest("title", &title, TITLE_MAX)?;
            edit.insert("title".into(), json!(title));
        }
        for f in ["body"].iter().chain(RUNS.iter()) {
            if let Some(v) = input.get(*f).filter(|v| !v.is_null()) {
                edit.insert((*f).into(), v.clone());
            }
        }
        if let Some(more) = said(input, "append")? {
            if edit.contains_key("body") {
                return Err(
                    "Give its brief in full (body) or words to add to it (append), not both."
                        .into(),
                );
            }
            // Added to the brief its newest revision gives it: a change made
            // since that revision is refused as stale, never written over.
            let was = detail
                .revisions
                .last()
                .and_then(|r| r.snapshot.ticket(t.ticket.id))
                .map_or(t.ticket.body.clone(), |n| n.body.clone());
            let body = match was.trim_end() {
                "" => more,
                was => format!("{was}\n\n{more}"),
            };
            longest("brief", &body, BODY_MAX)?;
            edit.insert("body".into(), json!(body));
        }
        let why = input["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or("Give the reason: the person reads it in Activity.")?;
        let through = json!({
            "plan_id": p.id.to_string(),
            "revision": detail.summary.plan.revision,
            "tickets_edit": [Value::Object(edit)],
            "why": why,
        });
        let mut out = self.plan_tool(super::plan::UPDATE, b, &through).await?;
        let note = out["note"].as_str().unwrap_or_default().to_string();
        out["note"] = json!(format!(
            "{key} is a ticket of your plan, so this went through the plan. {note}"
        )
        .trim()
        .to_string());
        Ok(Some(out))
    }

    /// Fills in the ticket a change is about, and what it names: with
    /// `seen`, only what that scope sees, and only on Desk's own boards.
    pub(crate) async fn resolve_change(
        &self,
        ticket: &mut String,
        id: &mut Option<Uuid>,
        change: &mut TicketChange,
        seen: Option<&Seen>,
    ) -> Result<(), String> {
        let t = self.ticket_named(ticket, *id, seen).await?;
        self.own_board_of(&t).await?;
        *id = Some(t.ticket.id);
        if let Some(key) = &t.key {
            ticket.clone_from(key);
        }
        let plan = self.open_plan_of(t.ticket.id).await;
        match change {
            TicketChange::Need {
                other, other_id, ..
            } => {
                let o = self.ticket_named(other, *other_id, seen).await?;
                *other_id = Some(o.ticket.id);
                if let Some(key) = &o.key {
                    other.clone_from(key);
                }
                // A plan keeps its own tickets' needs: one made here would
                // be undone at its next revision.
                if let (Some((p, _)), Some((q, _))) = (&plan, self.open_plan_of(o.ticket.id).await)
                {
                    if *p == q {
                        return Err(format!(
                            "{ticket} and {other} are both tickets of a plan: change what they need through it (crew.plan_update, the tickets' needs)."
                        ));
                    }
                }
            }
            // A plan's own ticket goes with the plan: deleting it would
            // leave the plan naming a ticket that isn't there.
            // Taken on from its board or not: the plan still names it.
            TicketChange::Delete if plan.is_some() => {
                return Err(format!(
                    "{ticket} is a ticket of an open plan: take it out through the plan (crew.plan_update), or drop it (crew.move_ticket)."
                ));
            }
            TicketChange::Edit {
                body: Some(_), was, ..
            } => {
                let now = digest(&t.ticket.body);
                match was {
                    None => *was = Some(now),
                    Some(w) if *w == now => {}
                    Some(_) => {
                        return Err(format!(
                            "{ticket}'s brief changed since this was suggested; read it again (crew.ticket) before replacing it."
                        ))
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The open plan (not done or dropped) ticket `id` is in, by its id,
    /// and whether the plan took it on from its board.
    pub(crate) async fn open_plan_of(&self, id: Uuid) -> Option<(Uuid, bool)> {
        let store = self.desk.store();
        let plan = store.plan_of_ticket(id).await.ok().flatten()?;
        let p = store.plan(plan).await.ok().flatten()?;
        if matches!(
            p.state,
            workforce_desk::plan::PlanState::Done | workforce_desk::plan::PlanState::Dropped
        ) {
            return None;
        }
        let adopted = store
            .plan_runs(plan)
            .await
            .ok()
            .and_then(|r| r.into_iter().find(|(t, _)| *t == id))
            .is_some_and(|(_, r)| r.adopted);
        Some((plan, adopted))
    }

    /// Fills in the board a new ticket goes on and the tickets it needs.
    pub(crate) async fn resolve_new(
        &self,
        new: &mut NewOne,
        seen: Option<&Seen>,
    ) -> Result<(), String> {
        let b = self
            .own_board_named(new.board.as_deref(), new.board_id, seen)
            .await?;
        new.board_id = Some(b.id);
        new.board = Some(b.prefix.clone().unwrap_or_else(|| b.name.clone()));
        // A suggestion's ids name the tickets it showed, by key.
        let kept = std::mem::take(&mut new.need_ids);
        for (i, k) in new.needs.iter().enumerate() {
            let t = self.ticket_named(k, kept.get(i).copied(), seen).await?;
            new.need_ids.push(t.ticket.id);
        }
        Ok(())
    }

    /// Makes the change `action` asks of a ticket, as `by`, through the
    /// calls the person's board makes.
    pub(crate) async fn change_ticket(&self, action: &Action, by: By) -> Result<String, String> {
        let plain = super::actions::board_plain;
        match action {
            Action::ChangeTicket { ticket, id, change } => {
                let (mut ticket, mut id, mut change) = (ticket.clone(), *id, (**change).clone());
                // Read again as it is now: a board linked since, or a ticket
                // gone, is refused here too.
                self.resolve_change(&mut ticket, &mut id, &mut change, None)
                    .await?;
                let id = id.ok_or("That ticket isn’t there any more.")?;
                match change {
                    TicketChange::Edit {
                        title,
                        body,
                        append,
                        ..
                    } => {
                        let added = append.is_some();
                        self.desk
                            .edit_ticket(
                                id,
                                TicketPatch {
                                    title,
                                    body,
                                    append,
                                    ..Default::default()
                                },
                                &by,
                            )
                            .await
                            .map_err(plain)?;
                        Ok(match added {
                            true => format!("added to {ticket}'s brief"),
                            false => format!("changed {ticket}"),
                        })
                    }
                    TicketChange::Comment { text } => {
                        self.desk
                            .post_ticket_comment(id, &text, &by, None)
                            .await
                            .map_err(|e| match e {
                                CommentPost::Desk(e) => plain(e),
                                other => other.to_string(),
                            })?;
                        Ok(format!("commented on {ticket}"))
                    }
                    TicketChange::Need {
                        other,
                        other_id,
                        needs,
                    } => {
                        let other_id = other_id.ok_or("That ticket isn’t there any more.")?;
                        self.desk
                            .need_ticket(id, other_id, needs, &by)
                            .await
                            .map_err(plain)?;
                        Ok(match needs {
                            true => format!("made {ticket} need {other}"),
                            false => format!("made {ticket} no longer need {other}"),
                        })
                    }
                    TicketChange::Delete => {
                        // The person's yes on the card: never a boss's,
                        // whatever path it takes.
                        if by.party.kind != PartyKind::You {
                            return Err(format!("Only the person deletes a ticket: {DELETE_WHY}."));
                        }
                        self.desk.delete_ticket(id, &by).await.map_err(plain)?;
                        Ok(format!("deleted {ticket}"))
                    }
                }
            }
            Action::CreateTicket { new } => {
                let mut new = new.clone();
                self.resolve_new(&mut new, None).await?;
                let board = new.board_id.ok_or("That board isn't there any more.")?;
                let v = self
                    .desk
                    .create_ticket(
                        NewTicket {
                            board,
                            title: new.title,
                            body: new.body,
                            state: Some(new.state),
                            needs: new.need_ids,
                        },
                        &by,
                    )
                    .await
                    .map_err(plain)?;
                Ok(format!(
                    "made {} “{}”",
                    v.key.as_deref().unwrap_or("a ticket"),
                    crew::one_line(&v.ticket.title, 120)
                ))
            }
            _ => Err("That isn't a change to a ticket.".into()),
        }
    }
}

/// The calls above as boxed futures, each made in a frame of its own: an
/// action's future is polled deep in a turn, and a debug build gives every
/// future built inline a stack slot of its own size in its caller's frame.
impl CrewHub {
    pub(super) fn ticket_tool_boxed<'a>(
        &'a self,
        b: &'a Bound,
        input: &'a Value,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(self.ticket_tool(b, input))
    }

    pub(super) fn edit_by_plan_boxed<'a>(
        &'a self,
        b: &'a Bound,
        input: &'a Value,
    ) -> BoxFuture<'a, Result<Option<Value>, String>> {
        Box::pin(self.edit_by_plan(b, input))
    }

    pub(crate) fn resolve_change_boxed<'a>(
        &'a self,
        ticket: &'a mut String,
        id: &'a mut Option<Uuid>,
        change: &'a mut TicketChange,
        seen: Option<&'a Seen>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.resolve_change(ticket, id, change, seen))
    }

    pub(crate) fn resolve_new_boxed<'a>(
        &'a self,
        new: &'a mut NewOne,
        seen: Option<&'a Seen>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.resolve_new(new, seen))
    }

    pub(crate) fn change_ticket_boxed<'a>(
        &'a self,
        action: &'a Action,
        by: By,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(self.change_ticket(action, by))
    }
}

/// A brief's digest, kept with a suggestion to replace it.
fn digest(text: &str) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}
