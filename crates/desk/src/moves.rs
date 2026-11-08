//! What moving an item to another sidebar group does (boss-gaps W8). One
//! move flow for sessions, tickets, rooms and bosses: the person sees what
//! the move does in plain words, chooses whether a session's ticket goes
//! with it, and reads (and may edit) the handover note before it is sent.
//!
//! Groups with a boss of their own are client boundaries
//! ([`crate::scope`]). Within one scope a move changes only the sidebar.
//! Across one, what was written stays where it was written: the ledger,
//! activity, lines, plans' revisions, session memory and the old boss's
//! memory keep the old group, and only what is written from then on is the
//! new group's. The handover note is the only thing that crosses, and only
//! once the person has read it.
//!
//! Pure: [`move_effects`] reads a [`MoveContext`] the service gathers.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::scope::scope_of;

/// The longest handover note that is delivered.
pub const MAX_NOTE: usize = 4000;

/// How long a preview stands.
pub const PREVIEW_FOR: chrono::Duration = chrono::Duration::minutes(10);

/// What is moved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Item {
    Session(Uuid),
    Ticket(Uuid),
    Room(Uuid),
    /// A boss, by its agent id: its `[oversight] group` changes.
    Boss(String),
}

impl Item {
    /// The item a [`Self::key`] names.
    pub fn parse(key: &str) -> Option<Self> {
        let (kind, id) = key.split_once(':')?;
        match kind {
            "session" => Uuid::parse_str(id).ok().map(Item::Session),
            "ticket" => Uuid::parse_str(id).ok().map(Item::Ticket),
            "room" => Uuid::parse_str(id).ok().map(Item::Room),
            "boss" if !id.is_empty() => Some(Item::Boss(id.to_string())),
            _ => None,
        }
    }

    /// How a handover names it: `session:<id>`, `ticket:<id>`, `room:<id>`
    /// or `boss:<agent>`.
    pub fn key(&self) -> String {
        match self {
            Item::Session(id) => format!("session:{id}"),
            Item::Ticket(id) => format!("ticket:{id}"),
            Item::Room(id) => format!("room:{id}"),
            Item::Boss(agent) => format!("boss:{agent}"),
        }
    }
}

/// What the person chose in the move dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Choices {
    /// A session's ticket goes with it to the new group's board; else it
    /// stays on its board, unassigned.
    #[serde(default)]
    pub ticket_goes: bool,
    /// The session's boss lets it go, rather than handing it to the new
    /// group's boss.
    #[serde(default)]
    pub release: bool,
    /// A boss moved into a group that has one replaces it.
    #[serde(default)]
    pub replace: bool,
}

/// A boss, as a move names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BossRef {
    pub agent: String,
    pub name: String,
}

/// A ticket of the item's, as a move weighs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketFact {
    pub id: Uuid,
    /// `ACM-12`; none on a synced board.
    pub key: Option<String>,
    pub board: Uuid,
    /// The board's group.
    pub board_group: Option<String>,
    /// Its board is synced with Linear or Jira (it has no prefix of its own).
    pub synced: bool,
    pub plan: Option<Uuid>,
    /// Open: not Done or Dropped.
    pub open: bool,
}

/// What a move reads, as things stand.
#[derive(Debug, Clone, Default)]
pub struct MoveContext {
    /// The item's group now (none: ungrouped). For a boss, the group of the
    /// scope it oversees.
    pub from: Option<String>,
    /// The groups that are boundaries.
    pub bossed: HashSet<String>,
    /// What each group is called.
    pub names: std::collections::HashMap<String, String>,
    /// The boss of the item's scope now, and of the scope it moves to (for
    /// everything ungrouped, the main boss).
    pub from_boss: Option<BossRef>,
    pub to_boss: Option<BossRef>,
    /// The main boss, overseeing everything ungrouped.
    pub main_boss: Option<BossRef>,
    /// The boss running the session, when one does.
    pub charged_by: Option<BossRef>,
    /// The session's tickets, or the ticket moved.
    pub tickets: Vec<TicketFact>,
    /// The new group's board a ticket would go to: its first with a prefix.
    pub to_board: Option<Uuid>,
    /// The session's open questions to its boss.
    pub open_asks: usize,
    /// The ticket moved is assigned to a session, which stays.
    pub assigned: bool,
    /// Of the ticket that goes: how many open tickets wait on it, and how
    /// many it waits on.
    pub needs: (usize, usize),
    /// The new scope's boss only watches: it runs nothing.
    pub to_boss_watches: bool,
    /// The item's own budget (`session:<id>`).
    pub own_budget: Option<Uuid>,
    /// What the item spent so far: ledger rows and dollars.
    pub ledger: (u64, f64),
    /// For a boss: the sessions it runs and its open plans.
    pub boss_charges: usize,
    pub boss_plans: Vec<Uuid>,
    /// The item is gone, ended, or otherwise can't be moved: why.
    pub refused: Option<String>,
}

impl MoveContext {
    fn name(&self, group: Option<&str>) -> String {
        match group {
            Some(g) => self.names.get(g).cloned().unwrap_or_else(|| g.to_string()),
            None => "no group".into(),
        }
    }
}

/// What a move does, beyond the sidebar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// The session's boss now is the new scope's.
    ChargeTo { boss: String },
    /// Its boss lets it go: the person runs it on their own.
    Release,
    /// Its ticket stays on the old board, back in To do and unassigned.
    StaysUnassigned { ticket: Uuid, synced: bool },
    /// The ticket goes to the new group's board.
    TicketGoes { ticket: Uuid, board: Uuid },
    /// A ticket of a plan leaves it.
    LeavesPlan { plan: Uuid, ticket: Uuid },
    /// A ticket moved without its session is taken off it.
    Unassigned { ticket: Uuid },
    /// What it spent so far stays in the old group's spend.
    LedgerStays { rows: u64, usd: f64 },
    /// Tickets of the board it leaves wait on it (`waiting`), or it on them
    /// (`waits_on`): each keeps waiting, on a ticket now in another group.
    NeedsKept { waiting: usize, waits_on: usize },
    /// Its own budget goes with it.
    BudgetFollows { budget: Uuid },
    /// Its open questions to the old boss are withdrawn.
    AsksWithdrawn { count: usize },
    /// The boss moved stops another from overseeing the group it moves to.
    Replaces { boss: String },
    /// The group it moves to has a boss: the person must say whether to
    /// replace them. Nothing moves until they do.
    AsksToReplace { boss: String },
    /// The group a boss leaves has no boss: its sessions and plans pass to
    /// the main boss, each plan paused until its note is read.
    PassesToMain { charges: usize, plans: usize },
    /// No one oversees what the boss leaves: its sessions go back to the
    /// person and its plans pause.
    LeftWithNoBoss { charges: usize, plans: usize },
    /// A boss keeps what it learned, but won't recall it in the new scope.
    MemoryStays,
    /// A handover note from one boss to the other.
    Handover { from: String, to: String },
    /// It is told on its line what changed.
    Tell,
    /// The move can't be made.
    Refused { why: String },
}

/// Everything `item` moving to `to` (a group; none: ungrouped) does, given
/// `ctx` and the person's `choices`. Nothing but the sidebar within one
/// scope; a refusal, when there is one, is the only effect.
pub fn move_effects(
    item: &Item,
    to: Option<&str>,
    ctx: &MoveContext,
    choices: &Choices,
) -> Vec<Effect> {
    if let Some(why) = &ctx.refused {
        return vec![Effect::Refused { why: why.clone() }];
    }
    let to = to.filter(|g| *g != crate::service::UNGROUPED);
    let from_scope = scope_of(ctx.from.as_deref(), &ctx.bossed);
    let to_scope = scope_of(to, &ctx.bossed);
    let across = from_scope != to_scope;
    match item {
        Item::Session(_) if !across => Vec::new(),
        Item::Session(_) => session_effects(ctx, choices),
        Item::Ticket(_) => ticket_effects(ctx, to, across),
        Item::Room(_) if !across => Vec::new(),
        Item::Room(_) => vec![Effect::Refused {
            why: format!(
                "A room keeps its conversation, so it can't move to a group another boss \
                 oversees. Start a new room in {} instead.",
                ctx.name(to)
            ),
        }],
        Item::Boss(_) => boss_effects(ctx, choices, to),
    }
}

fn session_effects(ctx: &MoveContext, choices: &Choices) -> Vec<Effect> {
    let mut out = Vec::new();
    for t in ctx.tickets.iter().filter(|t| t.open) {
        let goes = choices.ticket_goes && !t.synced && ctx.to_board.is_some();
        match (goes, ctx.to_board) {
            (true, Some(board)) => {
                if let Some(plan) = t.plan {
                    out.push(Effect::LeavesPlan { plan, ticket: t.id });
                }
                out.push(Effect::TicketGoes {
                    ticket: t.id,
                    board,
                });
                if ctx.needs != (0, 0) {
                    out.push(Effect::NeedsKept {
                        waiting: ctx.needs.0,
                        waits_on: ctx.needs.1,
                    });
                }
            }
            _ => out.push(Effect::StaysUnassigned {
                ticket: t.id,
                synced: t.synced,
            }),
        }
    }
    if ctx.open_asks > 0 {
        out.push(Effect::AsksWithdrawn {
            count: ctx.open_asks,
        });
    }
    if ctx.charged_by.is_some() {
        match ctx
            .to_boss
            .as_ref()
            .filter(|_| !choices.release && !ctx.to_boss_watches)
        {
            Some(b) => out.push(Effect::ChargeTo {
                boss: b.agent.clone(),
            }),
            None => out.push(Effect::Release),
        }
    }
    let (rows, usd) = ctx.ledger;
    if rows > 0 {
        out.push(Effect::LedgerStays { rows, usd });
    }
    if let Some(budget) = ctx.own_budget {
        out.push(Effect::BudgetFollows { budget });
    }
    if let Some(h) = handover(ctx.from_boss.as_ref(), ctx.to_boss.as_ref()) {
        out.push(h);
    }
    out.push(Effect::Tell);
    out
}

fn ticket_effects(ctx: &MoveContext, to: Option<&str>, across: bool) -> Vec<Effect> {
    let Some(t) = ctx.tickets.first() else {
        return vec![Effect::Refused {
            why: "There's no such ticket.".into(),
        }];
    };
    if t.board_group.as_deref() == to {
        return Vec::new();
    }
    let name = t.key.clone().unwrap_or_else(|| "This ticket".into());
    if t.synced {
        return vec![Effect::Refused {
            why: format!(
                "{name} lives on its board's Linear or Jira, so it can't move to another board."
            ),
        }];
    }
    let Some(board) = ctx.to_board else {
        return vec![Effect::Refused {
            why: format!(
                "{} has no board of its own to take {name}; make one first.",
                ctx.name(to)
            ),
        }];
    };
    let mut out = Vec::new();
    if let Some(plan) = t.plan {
        out.push(Effect::LeavesPlan { plan, ticket: t.id });
    }
    if ctx.assigned {
        out.push(Effect::Unassigned { ticket: t.id });
    }
    out.push(Effect::TicketGoes {
        ticket: t.id,
        board,
    });
    if ctx.needs != (0, 0) {
        out.push(Effect::NeedsKept {
            waiting: ctx.needs.0,
            waits_on: ctx.needs.1,
        });
    }
    if across {
        let (rows, usd) = ctx.ledger;
        if rows > 0 {
            out.push(Effect::LedgerStays { rows, usd });
        }
        if let Some(h) = handover(ctx.from_boss.as_ref(), ctx.to_boss.as_ref()) {
            out.push(h);
        }
    }
    out
}

fn boss_effects(ctx: &MoveContext, choices: &Choices, to: Option<&str>) -> Vec<Effect> {
    let from = ctx
        .from
        .as_deref()
        .filter(|g| *g != crate::service::UNGROUPED);
    if from == to {
        return Vec::new();
    }
    let Some(me) = ctx.from_boss.as_ref() else {
        return vec![Effect::Refused {
            why: "That agent doesn't oversee anything to move.".into(),
        }];
    };
    let mut out = Vec::new();
    // The one there now, unless it's this one.
    let there = ctx.to_boss.as_ref().filter(|b| b.agent != me.agent);
    match (there, choices.replace) {
        (Some(b), false) => {
            return vec![Effect::AsksToReplace {
                boss: b.agent.clone(),
            }]
        }
        (Some(b), true) => out.push(Effect::Replaces {
            boss: b.agent.clone(),
        }),
        (None, _) => {}
    }
    let plans = ctx.boss_plans.len();
    let charges = ctx.boss_charges;
    let main = ctx.main_boss.as_ref().filter(|b| b.agent != me.agent);
    match (from, main) {
        // A group it leaves passes to the main boss.
        (Some(_), Some(main)) => {
            if charges > 0 || plans > 0 {
                out.push(Effect::PassesToMain { charges, plans });
            }
            if plans > 0 {
                out.push(Effect::Handover {
                    from: me.agent.clone(),
                    to: main.agent.clone(),
                });
            }
        }
        _ => {
            if charges > 0 || plans > 0 {
                out.push(Effect::LeftWithNoBoss { charges, plans });
            }
        }
    }
    out.push(Effect::MemoryStays);
    out
}

fn handover(from: Option<&BossRef>, to: Option<&BossRef>) -> Option<Effect> {
    let to = to?;
    Some(Effect::Handover {
        from: from.map_or_else(|| "desk".to_string(), |b| b.agent.clone()),
        to: to.agent.clone(),
    })
}

/// Whether the move may go ahead as previewed: none of its effects
/// refuses it or still asks.
pub fn blocked(effects: &[Effect]) -> Option<String> {
    effects.iter().find_map(|e| match e {
        Effect::Refused { why } => Some(why.clone()),
        Effect::AsksToReplace { boss } => Some(format!(
            "The group already has {boss}; say whether to replace them first."
        )),
        _ => None,
    })
}

/// Who an effect names, by agent id: its name, as the person reads it.
pub trait Names {
    fn boss(&self, agent: &str) -> String;
    fn ticket(&self, id: Uuid) -> String;
    fn group(&self, group: Option<&str>) -> String;
}

/// What `e` does, in the words the move dialog shows; moving `item` from
/// `from` to `to`.
pub fn effect_text(
    e: &Effect,
    item: &str,
    from: Option<&str>,
    to: Option<&str>,
    n: &dyn Names,
) -> String {
    let (from, to) = (n.group(from), n.group(to));
    match e {
        Effect::ChargeTo { boss } => format!("{} runs {item} from now on.", n.boss(boss)),
        Effect::Release => format!("Its boss lets {item} go: you run it on your own."),
        Effect::StaysUnassigned { ticket, synced: true } => format!(
            "{} lives on its board's Linear or Jira. It stays there, unassigned.",
            n.ticket(*ticket)
        ),
        Effect::StaysUnassigned { ticket, .. } => format!(
            "{} stays on {from}'s board, back in To do and unassigned.",
            n.ticket(*ticket)
        ),
        Effect::TicketGoes { ticket, .. } => format!(
            "{} goes to {to}'s board with a new key; its history stays with {from}.",
            n.ticket(*ticket)
        ),
        Effect::LeavesPlan { ticket, .. } => {
            format!("{} leaves the plan it was in.", n.ticket(*ticket))
        }
        Effect::Unassigned { ticket } => {
            format!("{} is taken off the session working on it.", n.ticket(*ticket))
        }
        Effect::LedgerStays { usd, .. } => format!(
            "What it spent so far (${usd:.2}) stays in {from}'s spend; what it spends from now on is {to}'s."
        ),
        Effect::BudgetFollows { .. } => "Its own budget goes with it.".into(),
        Effect::NeedsKept { waiting, waits_on } => {
            let mut parts = Vec::new();
            if *waiting > 0 {
                parts.push(format!(
                    "{} on {from}'s board still wait on it",
                    plural(*waiting, "ticket")
                ));
            }
            if *waits_on > 0 {
                parts.push(format!("it still waits on {}", plural(*waits_on, "ticket")));
            }
            format!(
                "{}, as a ticket now in another group.",
                capitalised(&parts.join(", and "))
            )
        }
        Effect::AsksWithdrawn { count: 1 } => "Its question to its boss is withdrawn.".into(),
        Effect::AsksWithdrawn { count } => format!("Its {count} questions to its boss are withdrawn."),
        Effect::Replaces { boss } => format!("{} stops overseeing {to}.", n.boss(boss)),
        Effect::AsksToReplace { boss } => format!(
            "{to} already has {}. Replace them with {item}? {} stops overseeing.",
            n.boss(boss),
            n.boss(boss)
        ),
        Effect::PassesToMain { charges, plans } => format!(
            "{from} has no boss after this: its {} and {} pass to the main boss, and each plan waits until you've read its note.",
            plural(*charges, "session"),
            plural(*plans, "plan")
        ),
        Effect::LeftWithNoBoss { charges, plans } => format!(
            "Nobody oversees what {item} leaves: its {} go back to you and its {} pause.",
            plural(*charges, "session"),
            plural(*plans, "plan")
        ),
        Effect::MemoryStays => format!(
            "{item} keeps what they learned in {from}, but won't recall it while they run {to}."
        ),
        Effect::Handover { from: f, to: t } => format!(
            "{} receives only the note below from {}.",
            n.boss(t),
            match f.as_str() {
                "desk" => "Desk".to_string(),
                f => n.boss(f),
            }
        ),
        Effect::Tell => format!("{item} is told on its line that it moved."),
        Effect::Refused { why } => why.clone(),
    }
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn plural(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

/// What a handover note is written from when its boss doesn't write it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NoteFacts {
    /// What is handed over, as the person calls it.
    pub item: String,
    pub goal: Option<String>,
    /// `ACM-12 · Fix the login`.
    pub ticket: Option<String>,
    /// The gist of its last reply.
    pub last_reply: Option<String>,
    /// Its open questions.
    pub open_asks: Vec<String>,
    pub spend_usd: Option<f64>,
    /// What comes next, when known.
    pub next: Option<String>,
}

/// The note Desk writes from `f` when the old boss is absent, only
/// watches, or doesn't answer in time. Plain facts, no more.
pub fn facts_note(f: &NoteFacts) -> String {
    let mut lines = vec![format!("Handing over {}.", f.item)];
    if let Some(goal) = f.goal.as_deref().filter(|g| !g.trim().is_empty()) {
        lines.push(format!("Goal: {}", crate::crew::one_line(goal, 300)));
    }
    if let Some(t) = &f.ticket {
        lines.push(format!("Ticket: {t}"));
    }
    if let Some(r) = f.last_reply.as_deref().filter(|r| !r.trim().is_empty()) {
        lines.push(format!("Last reply: {}", crate::crew::one_line(r, 400)));
    }
    if !f.open_asks.is_empty() {
        lines.push(format!(
            "Open questions: {}",
            f.open_asks
                .iter()
                .map(|q| crate::crew::one_line(q, 200))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if let Some(usd) = f.spend_usd.filter(|u| *u > 0.0) {
        lines.push(format!("Spent so far: ${usd:.2}"));
    }
    if let Some(next) = &f.next {
        lines.push(format!("Next: {}", crate::crew::one_line(next, 300)));
    }
    clip_note(&lines.join("\n"))
}

/// `note` cut to [`MAX_NOTE`] characters.
pub fn clip_note(note: &str) -> String {
    match note.chars().count() > MAX_NOTE {
        true => note.chars().take(MAX_NOTE).collect(),
        false => note.to_string(),
    }
}

/// What a note sent must be: some text, at most [`MAX_NOTE`] characters.
pub fn check_note(note: &str) -> Result<(), String> {
    if note.trim().is_empty() {
        return Err("The handover note is empty; say something for the new boss to go on.".into());
    }
    if note.chars().count() > MAX_NOTE {
        return Err(format!(
            "The handover note is longer than {MAX_NOTE} characters; shorten it."
        ));
    }
    Ok(())
}

crate::model::text_enum!(MoveState {
    Previewed => "previewed",
    Done => "done",
    Dropped => "dropped",
});

crate::model::text_enum!(HandoverState {
    Draft => "draft",
    Delivered => "delivered",
    Discarded => "discarded",
});

/// A move as previewed: what it does, and until when it stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveRow {
    pub id: Uuid,
    pub item: Item,
    pub from_group: Option<String>,
    pub to_group: Option<String>,
    pub choices: Choices,
    pub effects: Vec<Effect>,
    pub state: MoveState,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub done_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A handover note from the boss an item leaves to the boss it goes to.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Handover {
    pub id: Uuid,
    #[serde(rename = "move")]
    pub move_id: Uuid,
    /// What it is about: an item's key, or `plan:<id>`.
    pub item: String,
    pub from_group: Option<String>,
    pub to_group: Option<String>,
    pub from_boss: Option<String>,
    pub to_boss: Option<String>,
    pub note: String,
    /// The old boss, or Desk from the facts.
    pub drafted_by: crate::crew::Party,
    /// Until when the old boss may still draft it.
    pub asked_until: Option<chrono::DateTime<chrono::Utc>>,
    pub edited: bool,
    pub state: HandoverState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Handover {
    /// Whether its old boss is still writing it.
    pub fn drafting(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.state == HandoverState::Draft
            && self.drafted_by.kind == crate::crew::PartyKind::Desk
            && self.asked_until.is_some_and(|until| now < until)
    }
}

#[cfg(test)]
#[path = "moves_tests.rs"]
mod tests;
