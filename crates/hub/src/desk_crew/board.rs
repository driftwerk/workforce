//! The overseer and the board: `crew.board`, which reads the tickets that
//! are ready, being done or blocked, and the two things on the board that
//! wake the overseer: a ticket that became ready with no one on it, and a
//! session whose turn ended with its ticket still in Doing. Handing tickets
//! out and moving them are `crew.assign_ticket` and `crew.move_ticket`
//! ([`super::actions`]), behind the same gate as the rest.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight};
use workforce_desk::board::TicketState;
use workforce_desk::crew::{self, PartyKind};
use workforce_desk::{TicketQuery, TicketView};

use super::tools::cell;
use super::watcher::{reported, Trigger};
use super::CrewHub;

pub const NAME: &str = "crew.board";

pub const DESCRIPTION: &str = "The board's tickets that matter now, by board: the ready ones (to do, \
with everything they need done), the ones being done, and the blocked ones (to do, waiting on \
tickets that aren't done), each with its key (like ACM-12), who has it (with their session or agent \
id) and what it waits on. Hand a ready ticket out with crew.assign_ticket and move one with \
crew.move_ticket, both by its key.";

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

/// Which of the board's lists a ticket is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Column {
    Ready,
    Doing,
    Blocked,
}

impl Column {
    fn word(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
        }
    }
}

/// The list `v` is on, when it is on one: a snoozed ticket is off the
/// board, and Backlog, Review, Done and Dropped are none of the three.
fn column(v: &TicketView) -> Option<Column> {
    if v.snoozed {
        return None;
    }
    match v.ticket.state {
        TicketState::Doing => Some(Column::Doing),
        TicketState::Todo if v.ready => Some(Column::Ready),
        TicketState::Todo => Some(Column::Blocked),
        _ => None,
    }
}

/// Who has a ticket, with the id the overseer addresses them by.
fn holder(v: &TicketView) -> String {
    match &v.ticket.assignee {
        None => String::new(),
        Some(a) => match a.kind {
            PartyKind::Session => format!("{} (session {})", a.name, a.id),
            PartyKind::Agent => format!("{} (agent {})", a.name, a.id),
            _ => a.name.clone(),
        },
    }
}

/// The tickets of one board as a table, in the order of their columns and
/// then of the board. `waits` names what each blocked ticket waits on.
fn table(views: &[&TicketView], waits: &HashMap<Uuid, String>) -> String {
    let mut rows: Vec<(Column, &TicketView)> = views
        .iter()
        .filter_map(|v| column(v).map(|c| (c, *v)))
        .collect();
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.ticket.rank.total_cmp(&b.1.ticket.rank))
    });
    let mut out =
        String::from("| Key | Title | Column | Has it | Waits on |\n|---|---|---|---|---|\n");
    for (c, v) in rows {
        let waiting = v
            .blockers
            .iter()
            .filter_map(|b| waits.get(b).map(String::as_str))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            v.key.as_deref().unwrap_or("-"),
            cell(&v.ticket.title),
            c.word(),
            cell(&holder(v)),
            cell(&waiting),
        ));
    }
    out
}

impl CrewHub {
    /// `crew.board`: what is ready, being done and blocked, board by board.
    pub(super) async fn board(&self) -> Result<Value, String> {
        let internal = |e: workforce_desk::DeskError| {
            tracing::warn!(error = %e, "reading the board for crew.board failed");
            "The board could not be read just now.".to_string()
        };
        let mut counts: HashMap<Column, usize> = HashMap::new();
        let mut out = String::new();
        for b in self.desk.boards().await.map_err(internal)? {
            let views = self
                .desk
                .tickets(b.id, &TicketQuery::default())
                .await
                .map_err(internal)?;
            let on: Vec<&TicketView> = views.iter().filter(|v| column(v).is_some()).collect();
            if on.is_empty() {
                continue;
            }
            for v in &on {
                if let Some(c) = column(v) {
                    *counts.entry(c).or_default() += 1;
                }
            }
            let blockers: Vec<Uuid> = on.iter().flat_map(|v| v.blockers.clone()).collect();
            let waits = match blockers.is_empty() {
                true => HashMap::new(),
                false => self
                    .desk
                    .store()
                    .ticket_keys(&blockers)
                    .await
                    .map_err(|e| internal(workforce_desk::DeskError::Internal(e)))?,
            };
            let name = match &b.prefix {
                Some(p) => format!("{} ({p})", b.name),
                None => b.name.clone(),
            };
            out.push_str(&format!(
                "Board “{}”:\n{}\n",
                cell(&name),
                table(&on, &waits)
            ));
        }
        if out.is_empty() {
            out.push_str("No ticket is ready, being done or blocked.");
        }
        let count = |c: Column| counts.get(&c).copied().unwrap_or(0);
        Ok(json!({
            "counts": {
                "ready": count(Column::Ready),
                "doing": count(Column::Doing),
                "blocked": count(Column::Blocked),
            },
            "table": reported(&out, "the board's tickets"),
        }))
    }

    /// `line`, the wake for `session`'s turn ending, with its ticket if that
    /// is still in Doing.
    pub(super) async fn with_doing(&self, session: Uuid, line: String) -> String {
        match self.desk.doing_key(session).await {
            Some(key) => format!("{line}; {key} is still Doing"),
            None => line,
        }
    }

    /// The tickets that became ready with no one on them since `seen` last
    /// looked, as wakes when the overseer may hand work out. `seen` keeps,
    /// for each board, the ready tickets nobody has, so that each is said
    /// once as it becomes so; it is kept up to date whether or not the
    /// overseer may hand work out, so that switching that on doesn't wake it
    /// for what has long been waiting.
    pub(super) async fn ready_unhanded(
        &self,
        seen: &mut HashMap<Uuid, HashSet<Uuid>>,
        o: &Oversight,
    ) -> Vec<Trigger> {
        let Ok(boards) = self.desk.boards().await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for b in boards {
            let Ok(views) = self.desk.tickets(b.id, &TicketQuery::default()).await else {
                continue;
            };
            let unhanded: Vec<&TicketView> = views
                .iter()
                .filter(|v| v.ready && v.ticket.assignee.is_none() && !v.snoozed)
                .collect();
            let known = seen.entry(b.id).or_default();
            let fresh: Vec<&TicketView> = unhanded
                .iter()
                .filter(|v| !known.contains(&v.ticket.id))
                .copied()
                .collect();
            *known = unhanded.iter().map(|v| v.ticket.id).collect();
            if o.level == Level::Watch || !o.act.hand_out_work {
                continue;
            }
            out.extend(fresh.into_iter().map(|v| Trigger {
                line: format!(
                    "{} “{}” is ready on the board and has no one on it yet",
                    v.key.as_deref().unwrap_or("A ticket"),
                    crew::one_line(&v.ticket.title, 80),
                ),
                approvals: Vec::new(),
                hop: 0,
                about: None,
            }));
        }
        out
    }
}
