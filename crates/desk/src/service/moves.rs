//! Moving an item to another sidebar group, as the person confirms it
//! (boss-gaps W8): a preview says what the move does and drafts the
//! handover note; the move is made only as previewed, and the note, as the
//! person sent it, is the only thing that crosses to the new boss.
//!
//! What the move takes away is done first (a ticket left behind, questions
//! withdrawn, the old boss letting the session go), then the sidebar
//! changes, which is what lets the new scope see the item, then what the
//! new scope gains (a ticket that goes along, the new boss's charge), and
//! the note last. A sidebar drag that isn't a confirmed move does what it
//! did before ([`Desk::moved_across`]): the ticket stays, and no note goes.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::scopes::IN_GROUP;
use super::{session_item, Desk, DeskError, UNGROUPED};
use crate::board::{Board, TicketState};
use crate::charge::{Charge, How};
use crate::crew::{self, By, Event, Kind, Party, PartyKind};
use crate::line::{Asked, Delivery, LineKind, LineMessage};
use crate::model::DeskEvent;
use crate::move_store::Sent;
use crate::moves::{
    self, BossRef, Choices, Effect, Handover, HandoverState, Item, MoveContext, MoveRow, MoveState,
    NoteFacts, TicketFact,
};
use crate::plan::PlanState;
use crate::scope::{scope_of, Scope};
use workforce_core::budget::BudgetScope;

/// How long the boss an item leaves has to draft its note.
pub const DRAFT_FOR: chrono::Duration = chrono::Duration::seconds(60);

/// What the person asks to preview.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveAsk {
    pub item: Item,
    /// The group it goes to; none (or `ungrouped`) is no group.
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub choices: Choices,
}

/// One effect, with what the dialog says of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectView {
    #[serde(flatten)]
    pub effect: Effect,
    pub text: String,
}

/// A handover note as the dialog shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoverView {
    #[serde(flatten)]
    pub handover: Handover,
    /// Its old boss is still writing it: the note may change.
    pub drafting: bool,
}

/// What a move would do, for the person to confirm.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovePreview {
    pub id: Uuid,
    pub item: Item,
    pub from: Option<String>,
    pub to: Option<String>,
    pub choices: Choices,
    pub effects: Vec<EffectView>,
    /// Why it can't be made as it is, when it can't.
    pub blocked: Option<String>,
    /// The item's own note, when it has one; a boss's plans have one each
    /// in `handovers`.
    pub handover: Option<HandoverView>,
    pub handovers: Vec<HandoverView>,
    pub expires_at: DateTime<Utc>,
}

/// A note as the person sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentNote {
    pub id: Uuid,
    pub note: String,
}

/// The person makes a previewed move.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveCommit {
    pub preview: Uuid,
    #[serde(default)]
    pub choices: Choices,
    /// The item's own note, as the person read and perhaps edited it.
    #[serde(default)]
    pub note: Option<String>,
    /// Every other note, by its id.
    #[serde(default)]
    pub notes: Vec<SentNote>,
}

/// What a move did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveDone {
    pub effects: Vec<EffectView>,
    /// The notes delivered.
    pub delivered: Vec<Handover>,
    /// What didn't go as it should, in plain words.
    pub problems: Vec<String>,
}

/// Effects as a preview and its move are compared: what a session spends
/// meanwhile doesn't change what the move does.
fn comparable(effects: &[Effect]) -> Vec<&Effect> {
    effects
        .iter()
        .filter(|e| !matches!(e, Effect::LedgerStays { .. }))
        .collect()
}

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

const STALE: &str = "What this move does has changed since you looked; look at it again.";
const OUT_OF_DATE: &str = "This move was made already or has run out; look at it again.";

/// The names an effect's words use.
struct Names<'a> {
    desk: &'a Desk,
    tickets: HashMap<Uuid, String>,
}

impl moves::Names for Names<'_> {
    fn boss(&self, agent: &str) -> String {
        self.desk.named_boss(agent)
    }
    fn ticket(&self, id: Uuid) -> String {
        self.tickets
            .get(&id)
            .cloned()
            .unwrap_or_else(|| "Its ticket".into())
    }
    fn group(&self, group: Option<&str>) -> String {
        match group {
            Some(g) => self.desk.group_name(g).unwrap_or_else(|| g.to_string()),
            None => "no group".into(),
        }
    }
}

impl Desk {
    /// What the boss `agent` is called: as the hub said of its bosses, else
    /// as its roster and charges say.
    fn named_boss(&self, agent: &str) -> String {
        self.bosses()
            .into_iter()
            .find(|b| b.agent == agent)
            .map(|b| b.name)
            .filter(|n| !n.trim().is_empty() && n != agent)
            .unwrap_or_else(|| self.boss_name(agent))
    }

    /// `to` as a move reads it: a sidebar group, or none for no group.
    fn move_target(&self, to: Option<&str>) -> Result<Option<String>, DeskError> {
        match to
            .map(str::trim)
            .filter(|g| !g.is_empty() && *g != UNGROUPED)
        {
            None => Ok(None),
            Some(g) if self.is_group(g) => Ok(Some(g.to_string())),
            Some(g) => Err(DeskError::Invalid(format!(
                "there's no group {g} in the sidebar"
            ))),
        }
    }

    fn boss_ref(&self, scope: &Scope) -> Option<BossRef> {
        self.boss_of(scope).map(|b| BossRef {
            agent: b.agent,
            name: b.name,
        })
    }

    /// The facts a ticket is moved by.
    async fn ticket_fact(&self, t: &crate::board::Ticket, board: &Board) -> TicketFact {
        let plan = self.0.store.plan_of_ticket(t.id).await.ok().flatten();
        TicketFact {
            id: t.id,
            key: t.key(board),
            board: board.id,
            board_group: board.group.clone(),
            synced: board.prefix.is_none(),
            plan,
            open: !matches!(t.state, TicketState::Done | TicketState::Dropped),
        }
    }

    /// The first board of `group` a ticket can go to: one with keys of its
    /// own (not synced), oldest first.
    async fn board_for(&self, group: Option<&str>) -> Result<Option<Uuid>, DeskError> {
        let mut boards: Vec<Board> = self
            .0
            .store
            .boards()
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|b| b.group.as_deref() == group && b.prefix.is_some())
            .collect();
        boards.sort_by_key(|b| b.created_at);
        Ok(boards.first().map(|b| b.id))
    }

    /// The open questions session `id` asked its boss.
    async fn asks_to_boss(&self, id: Uuid) -> Result<Vec<LineMessage>, DeskError> {
        Ok(self
            .0
            .store
            .line_open_asks(None)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|a| {
                a.from_session() == Some(id)
                    && a.to.kind == PartyKind::Manager
                    && a.asked == Some(Asked::Open)
            })
            .collect())
    }

    /// What moving `item` to `to` reads, as things stand.
    async fn move_context(&self, item: &Item, to: Option<&str>) -> Result<MoveContext, DeskError> {
        let bossed = self.bossed();
        let mut ctx = MoveContext {
            bossed: bossed.clone(),
            names: self
                .layout()
                .groups
                .iter()
                .map(|g| (g.id.clone(), g.name.clone()))
                .collect(),
            main_boss: self.boss_ref(&Scope::Ungrouped),
            ..MoveContext::default()
        };
        match item {
            Item::Session(id) => {
                let view = self.view(*id)?;
                ctx.from = view.group.map(|g| g.id);
                ctx.charged_by = self.charge_of(*id).filter(Charge::live).map(|c| BossRef {
                    name: self.named_boss(&c.boss),
                    agent: c.boss,
                });
                let was = self.seen_by(&scope_of(ctx.from.as_deref(), &bossed));
                for t in self.0.store.session_tickets(*id).await.map_err(internal)? {
                    let board = self.board_of(t.board).await?;
                    // Only its tickets of the scope it leaves.
                    if was.sees(board.group.as_deref()) {
                        ctx.tickets.push(self.ticket_fact(&t, &board).await);
                    }
                }
                ctx.open_asks = self.asks_to_boss(*id).await?.len();
                ctx.own_budget = self
                    .budget_list()
                    .into_iter()
                    .find(|b| b.scope == BudgetScope::Session(*id))
                    .map(|b| b.id);
                ctx.ledger = self
                    .0
                    .store
                    .ledger_total(Some(*id), None)
                    .await
                    .map_err(internal)?;
            }
            Item::Ticket(id) => {
                let t = self.0.store.ticket(*id).await.map_err(internal)?;
                let Some(t) = t else {
                    return Err(DeskError::NotFound);
                };
                let board = self.board_of(t.board).await?;
                ctx.from = board.group.clone();
                ctx.assigned = t.session().is_some();
                ctx.tickets.push(self.ticket_fact(&t, &board).await);
                ctx.ledger = self
                    .0
                    .store
                    .ledger_total(None, Some(*id))
                    .await
                    .map_err(internal)?;
            }
            Item::Room(id) => {
                ctx.from = self.room_of(*id)?.group;
            }
            Item::Boss(agent) => {
                let Some(scope) = self.scope_of_boss(agent) else {
                    ctx.refused = Some("That agent doesn't oversee anything to move.".into());
                    return Ok(ctx);
                };
                ctx.from = scope.group().map(str::to_string);
                ctx.from_boss = Some(BossRef {
                    agent: agent.clone(),
                    name: self.named_boss(agent),
                });
                // The boss of the group it goes to, not of its scope: a
                // group with no boss yet is no one's.
                ctx.to_boss = match to {
                    Some(g) => self.boss_ref(&Scope::Group(g.to_string())),
                    None => self.boss_ref(&Scope::Ungrouped),
                };
                ctx.boss_charges = self
                    .charges()
                    .iter()
                    .filter(|c| c.boss == *agent && c.released_at.is_none())
                    .count();
                ctx.boss_plans = self
                    .0
                    .store
                    .plans(None)
                    .await
                    .map_err(internal)?
                    .into_iter()
                    .filter(|p| {
                        p.boss == *agent
                            && matches!(
                                p.state,
                                PlanState::Proposed | PlanState::Running | PlanState::Paused
                            )
                    })
                    .map(|p| p.id)
                    .collect();
                return Ok(ctx);
            }
        }
        // What waits on the ticket that would go, and what it waits on.
        if let Some(t) = ctx.tickets.iter().find(|t| t.open) {
            let edges = self.0.store.ticket_edges().await.map_err(internal)?;
            ctx.needs = (
                edges.iter().filter(|(_, needs)| *needs == t.id).count(),
                edges.iter().filter(|(ticket, _)| *ticket == t.id).count(),
            );
        }
        ctx.from_boss = self.boss_ref(&scope_of(ctx.from.as_deref(), &bossed));
        let to_scope = scope_of(to, &bossed);
        ctx.to_boss = self.boss_ref(&to_scope);
        ctx.to_boss_watches = self.boss_of(&to_scope).is_some_and(|b| b.level == "watch");
        ctx.to_board = match item {
            Item::Session(_) | Item::Ticket(_) => self.board_for(to).await?,
            _ => None,
        };
        Ok(ctx)
    }

    /// The effects with the words the dialog shows.
    async fn effect_views(
        &self,
        item: &Item,
        ctx: &MoveContext,
        to: Option<&str>,
        effects: &[Effect],
    ) -> Vec<EffectView> {
        let mut tickets = HashMap::new();
        for t in &ctx.tickets {
            tickets.insert(t.id, t.key.clone().unwrap_or_else(|| "Its ticket".into()));
        }
        let names = Names {
            desk: self,
            tickets,
        };
        let label = self.item_label(item).await;
        effects
            .iter()
            .map(|e| EffectView {
                text: moves::effect_text(e, &label, ctx.from.as_deref(), to, &names),
                effect: e.clone(),
            })
            .collect()
    }

    /// What the person calls `item`.
    pub(super) async fn item_label(&self, item: &Item) -> String {
        match item {
            Item::Session(id) => self
                .view(*id)
                .map(|v| v.label().to_string())
                .unwrap_or_else(|_| "the session".into()),
            Item::Ticket(id) => match self.0.store.ticket(*id).await.ok().flatten() {
                Some(t) => match self.key_of(&t).await {
                    Ok(key) => format!("{key} · {}", crew::one_line(&t.said_title(), 80)),
                    Err(_) => crew::one_line(&t.said_title(), 80),
                },
                None => "the ticket".into(),
            },
            Item::Room(id) => self
                .room_of(*id)
                .map(|r| r.name)
                .unwrap_or_else(|_| "the room".into()),
            Item::Boss(agent) => self.named_boss(agent),
        }
    }

    /// What Desk writes in a note about `item` from the facts.
    async fn item_facts(&self, item: &Item, ctx: &MoveContext) -> NoteFacts {
        let mut f = NoteFacts {
            item: self.item_label(item).await,
            spend_usd: Some(ctx.ledger.1),
            ..NoteFacts::default()
        };
        if let Item::Session(id) = item {
            if let Ok(v) = self.view(*id) {
                f.goal = Some(v.goal.clone());
                f.last_reply = v.last_reply.clone();
            }
            f.open_asks = self
                .asks_to_boss(*id)
                .await
                .unwrap_or_default()
                .iter()
                .map(|a| a.headline().to_string())
                .collect();
        }
        if let Some(t) = ctx.tickets.iter().find(|t| t.open) {
            if let Ok(Some(ticket)) = self.0.store.ticket(t.id).await {
                f.ticket = Some(match &t.key {
                    Some(k) => format!("{k} · {}", crew::one_line(&ticket.said_title(), 120)),
                    None => crew::one_line(&ticket.said_title(), 120),
                });
            }
        }
        f
    }

    /// What Desk writes in a note about plan `id` from the facts.
    async fn plan_facts(&self, id: Uuid) -> NoteFacts {
        match self.plan(id).await {
            Ok(p) => NoteFacts {
                item: format!("the plan “{}”", crew::one_line(&p.summary.plan.goal, 200)),
                goal: Some(p.summary.plan.goal.clone()),
                next: Some(format!(
                    "{} of {} done, {} doing{}",
                    p.summary.progress.done,
                    p.summary.progress.total,
                    p.summary.progress.doing,
                    match p.summary.progress.next.is_empty() {
                        true => String::new(),
                        false => format!(", next {}", p.summary.progress.next.join(", ")),
                    }
                )),
                ..NoteFacts::default()
            },
            Err(_) => NoteFacts {
                item: "a plan".into(),
                ..NoteFacts::default()
            },
        }
    }

    /// Previews moving `ask.item` to `ask.to`: what it does, and its
    /// handover notes, drafted from the facts and, where its old boss can,
    /// asked of it. Nothing changes until the person makes the move.
    pub async fn preview_move(&self, ask: MoveAsk) -> Result<MovePreview, DeskError> {
        let to = self.move_target(ask.to.as_deref())?;
        // Previews that ran out a day ago go, with their drafts.
        if let Err(e) = self.0.store.forget_old_moves().await {
            tracing::debug!(error = %e, "old move previews weren't cleared");
        }
        let ctx = self.move_context(&ask.item, to.as_deref()).await?;
        let effects = moves::move_effects(&ask.item, to.as_deref(), &ctx, &ask.choices);
        let now = crew::now();
        let row = MoveRow {
            id: Uuid::new_v4(),
            item: ask.item.clone(),
            from_group: ctx.from.clone(),
            to_group: to.clone(),
            choices: ask.choices,
            effects: effects.clone(),
            state: MoveState::Previewed,
            expires_at: now + moves::PREVIEW_FOR,
            created_at: now,
            done_at: None,
        };
        let mut drafts: Vec<(Handover, String)> = Vec::new();
        let blocked = moves::blocked(&effects);
        if blocked.is_none() {
            for e in &effects {
                let Effect::Handover { from, to: to_boss } = e else {
                    continue;
                };
                let about: Vec<(String, NoteFacts)> = match &ask.item {
                    Item::Boss(_) => {
                        let mut out = Vec::new();
                        for p in &ctx.boss_plans {
                            out.push((format!("plan:{p}"), self.plan_facts(*p).await));
                        }
                        out
                    }
                    item => vec![(item.key(), self.item_facts(item, &ctx).await)],
                };
                let from_boss = (from != "desk").then(|| from.clone());
                // The old boss is asked to write it unless it only watches.
                let asks = from_boss
                    .as_deref()
                    .and_then(|b| self.bosses().into_iter().find(|x| x.agent == b))
                    .is_some_and(|b| b.level != "watch");
                for (key, facts) in about {
                    let note = moves::facts_note(&facts);
                    drafts.push((
                        Handover {
                            id: Uuid::new_v4(),
                            move_id: row.id,
                            item: key,
                            from_group: ctx.from.clone(),
                            to_group: match &ask.item {
                                Item::Boss(_) => None,
                                _ => to.clone(),
                            },
                            from_boss: from_boss.clone(),
                            to_boss: Some(to_boss.clone()),
                            note: note.clone(),
                            drafted_by: Party::desk(),
                            asked_until: asks.then(|| now + DRAFT_FOR),
                            edited: false,
                            state: HandoverState::Draft,
                            created_at: now,
                            delivered_at: None,
                        },
                        note,
                    ));
                }
            }
        }
        let handovers: Vec<Handover> = drafts.iter().map(|(h, _)| h.clone()).collect();
        self.0
            .store
            .add_move(&row, &handovers)
            .await
            .map_err(internal)?;
        let crew = self.crew();
        for (h, facts) in drafts.into_iter().filter(|(h, _)| h.asked_until.is_some()) {
            if let Some(crew) = crew.clone() {
                tokio::spawn(async move { crew.ask_handover(&h, &facts).await });
            }
        }
        let views: Vec<HandoverView> = handovers
            .into_iter()
            .map(|h| HandoverView {
                drafting: h.asked_until.is_some(),
                handover: h,
            })
            .collect();
        let own = views
            .iter()
            .find(|v| v.handover.item == ask.item.key())
            .cloned();
        Ok(MovePreview {
            id: row.id,
            effects: self
                .effect_views(&ask.item, &ctx, to.as_deref(), &effects)
                .await,
            item: ask.item,
            from: ctx.from,
            to,
            choices: ask.choices,
            blocked,
            handover: own,
            handovers: views,
            expires_at: row.expires_at,
        })
    }

    fn crew(&self) -> Option<std::sync::Arc<dyn crew::Crew>> {
        self.0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    /// Handover note `id` as it stands, with whether it is still drafted.
    pub async fn handover_view(&self, id: Uuid) -> Result<HandoverView, DeskError> {
        let h = self
            .0
            .store
            .handover(id)
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)?;
        Ok(HandoverView {
            drafting: h.drafting(crew::now()),
            handover: h,
        })
    }

    /// The boss `boss` drafts note `id`, which it was asked to: only one of
    /// its own, a draft of a move still previewed, in time. The person
    /// reads it before it goes anywhere. Not found for any other.
    pub async fn handover_drafted(
        &self,
        id: Uuid,
        boss: &Party,
        note: &str,
    ) -> Result<Handover, DeskError> {
        moves::check_note(note).map_err(DeskError::Invalid)?;
        self.no_secret_in(note)?;
        let h = self
            .0
            .store
            .draft_handover(id, boss, note, crew::now())
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)?;
        self.announce(DeskEvent::Handover {
            handover: Box::new(h.clone()),
        });
        Ok(h)
    }

    /// `Err` when `note` holds what reads as a secret, or Desk can't tell.
    fn no_secret_in(&self, note: &str) -> Result<(), DeskError> {
        let Some(redactor) = self.0.ledger.redactor.as_ref() else {
            return Err(DeskError::Conflict(
                "Desk can't check the note for secrets yet; try again shortly.".into(),
            ));
        };
        match redactor.stored(note).count {
            0 => Ok(()),
            _ => Err(DeskError::Invalid(
                "The handover note has what looks like a secret in it (a key, token or \
                 password); take it out first."
                    .into(),
            )),
        }
    }

    /// Makes previewed move `c.preview`, as previewed: refused (409) when
    /// it ran out, was made, or what it does changed since. Each note goes
    /// as the person sent it, and only then.
    pub async fn commit_move(&self, c: MoveCommit) -> Result<MoveDone, DeskError> {
        let _one = self.0.move_write.lock().await;
        let row = self
            .0
            .store
            .move_row(c.preview)
            .await
            .map_err(internal)?
            .ok_or_else(|| DeskError::Conflict(OUT_OF_DATE.into()))?;
        let now = crew::now();
        if row.state != MoveState::Previewed || row.expires_at <= now {
            return Err(DeskError::Conflict(OUT_OF_DATE.into()));
        }
        let to = self.move_target(row.to_group.as_deref())?;
        let ctx = self.move_context(&row.item, to.as_deref()).await?;
        let effects = moves::move_effects(&row.item, to.as_deref(), &ctx, &c.choices);
        if c.choices != row.choices
            || comparable(&effects) != comparable(&row.effects)
            || ctx.from != row.from_group
        {
            return Err(DeskError::Conflict(STALE.into()));
        }
        if let Some(why) = moves::blocked(&effects) {
            return Err(DeskError::Conflict(why));
        }
        // Every note, as the person sent it: never a draft written since.
        let drafts = self.0.store.handovers_of(row.id).await.map_err(internal)?;
        let mut sent = Vec::new();
        for h in drafts.iter().filter(|h| h.state == HandoverState::Draft) {
            let text = match c.notes.iter().find(|n| n.id == h.id) {
                Some(n) => Some(n.note.clone()),
                None if h.item == row.item.key() => c.note.clone(),
                None => None,
            };
            let Some(text) = text else {
                return Err(DeskError::Invalid(
                    "Send each handover note as you read it.".into(),
                ));
            };
            moves::check_note(&text).map_err(DeskError::Invalid)?;
            self.no_secret_in(&text)?;
            sent.push(Sent {
                id: h.id,
                edited: text != h.note,
                note: text,
            });
        }
        // A session the sidebar can't take there is refused before
        // anything is done.
        if let Item::Session(id) = &row.item {
            let key = session_item(*id);
            let g = to.as_deref().unwrap_or(UNGROUPED);
            self.layout().put(&key, g, None)?;
        }
        let Some(delivered) = self
            .0
            .store
            .finish_move(row.id, &sent, now)
            .await
            .map_err(internal)?
        else {
            return Err(DeskError::Conflict(OUT_OF_DATE.into()));
        };
        let by = By {
            party: Party::you(),
            reason: Some("You moved it.".into()),
            suggestion_id: None,
            hop: 0,
        };
        let mut problems = Vec::new();
        let made = match &row.item {
            Item::Session(id) => {
                self.move_session(*id, to.as_deref(), &effects, &by, &mut problems)
                    .await
            }
            Item::Ticket(id) => {
                self.move_ticket_to(*id, to.as_deref(), &effects, &by, &mut problems)
                    .await
            }
            Item::Room(id) => self
                .update_room(
                    *id,
                    crate::rooms::RoomPatch {
                        group: Some(to.clone()),
                        ..Default::default()
                    },
                )
                .await
                .map(drop),
            Item::Boss(agent) => {
                self.move_boss_now(
                    agent,
                    to.as_deref(),
                    &c.choices,
                    &effects,
                    &by,
                    &mut problems,
                )
                .await
            }
        };
        if let Err(e) = made {
            // Nothing of it was delivered: the notes are set aside with it.
            if let Err(e) = self.0.store.drop_move(row.id).await {
                tracing::warn!(error = %e, "a move that failed wasn't set aside");
            }
            return Err(e);
        }
        self.moved_noted(&row.item, ctx.from.as_deref(), to.as_deref(), &by)
            .await;
        let mut handed = Vec::new();
        for h in delivered {
            match self.deliver_note(&h).await {
                Ok(()) => {
                    // A plan passed on waits for its note to be read.
                    if let Some(plan) = h.item.strip_prefix("plan:").and_then(|p| p.parse().ok()) {
                        if let Err(e) = self
                            .resume_plan(plan, &[crate::plan::Paused::NoBoss], &by)
                            .await
                        {
                            tracing::info!(%plan, error = %e, "a plan handed over stays paused");
                        }
                    }
                    handed.push(h);
                }
                Err(why) => problems.push(why),
            }
        }
        Ok(MoveDone {
            effects: self
                .effect_views(&row.item, &ctx, to.as_deref(), &effects)
                .await,
            delivered: handed,
            problems,
        })
    }

    async fn deliver_note(&self, h: &Handover) -> Result<(), String> {
        let Some(crew) = self.crew() else {
            return Err("The note couldn't be delivered: there's no boss here.".into());
        };
        crew.deliver_handover(h).await?;
        let to = h.to_boss.as_deref().unwrap_or_default();
        let e = Event::new(
            Kind::HandedOver,
            Party::you(),
            Some(Party::manager(to, &self.named_boss(to))),
            "handed over with a note",
        )
        .detail(json!({ IN_GROUP: h.to_group }));
        self.note(e);
        Ok(())
    }

    /// Notes the move in the scope it left and the one it went to: each
    /// side reads only its own side's names.
    async fn moved_noted(&self, item: &Item, from: Option<&str>, to: Option<&str>, by: &By) {
        let label = self.item_label(item).await;
        let subject = match item {
            Item::Session(id) => self.view(*id).ok().map(|v| Party::session(&v)),
            Item::Boss(agent) => Some(Party::manager(agent, &self.named_boss(agent))),
            _ => None,
        };
        // Each side reads only its own: the group it left never names the
        // one it went to.
        let there = match to {
            None => "no group",
            Some(_) => "another group",
        };
        self.note(
            by.event(
                Kind::Moved,
                subject.clone(),
                format!("moved {label} to {there}"),
            )
            .detail(json!({ IN_GROUP: from })),
        );
        if from != to {
            self.note(
                by.event(Kind::Moved, subject, format!("moved {label} here"))
                    .detail(json!({ IN_GROUP: to })),
            );
        }
    }

    /// Takes `ticket`, left behind by a session that moved, off it: back to
    /// To do when it was being worked on.
    async fn leave_ticket(&self, ticket: Uuid, by: &By) -> Result<(), DeskError> {
        let state = self.ticket(ticket).await?.ticket.state;
        match crate::board::unassigns(state) {
            true => self.assign_ticket(ticket, None, by).await.map(drop),
            false => {
                let mv = super::Move {
                    state: TicketState::Todo,
                    before: None,
                    after: None,
                };
                self.move_ticket(ticket, mv, by).await.map(drop)
            }
        }
    }

    /// Withdraws `asks`: their session moved from the boss they asked.
    async fn withdraw_asks(&self, asks: Vec<LineMessage>) -> Result<bool, DeskError> {
        let mut withdrawn = false;
        for mut ask in asks {
            ask.asked = Some(Asked::Withdrawn);
            ask.updated_at = Utc::now();
            self.0.store.put_line(&ask).await.map_err(internal)?;
            self.line_announce(&ask);
            withdrawn = true;
        }
        Ok(withdrawn)
    }

    /// Ticket `id` goes to board `board`: a new key there, out of its plan,
    /// its history left with the old board's group.
    async fn ticket_goes(&self, id: Uuid, board: Uuid, by: &By) -> Result<String, DeskError> {
        let before = self.ticket(id).await?;
        let old_board = self.board_of(before.ticket.board).await?;
        // A synced ticket's issue belongs to its board's link, in its
        // group: it never goes to another board.
        if let Some(e) = &before.ticket.external {
            self.leave_ticket(id, by).await?;
            return Err(DeskError::Conflict(format!(
                "{} lives on {}'s {}. It stays there, unassigned.",
                e.key,
                old_board.name,
                e.system.title()
            )));
        }
        let old_group = old_board.group;
        let moved = self
            .0
            .store
            .move_ticket_board(id, board)
            .await
            .map_err(internal)?
            .ok_or(DeskError::NotFound)?;
        let key = self.key_of(&moved).await?;
        // Said in the old board's history by its old key; in the new one's
        // by its new key alone.
        let old_key = before.key.clone().unwrap_or_else(|| "a ticket".into());
        self.note(
            by.event(
                Kind::TicketMoved,
                None,
                format!("moved {old_key} to another group's board"),
            )
            .about_ticket(id, old_group.as_deref()),
        );
        self.ticket_changed(
            moved,
            by,
            Kind::TicketMoved,
            None,
            format!("brought {key} here from another board"),
        )
        .await?;
        Ok(key)
    }

    /// Session `id` moves to `to`, doing `effects`.
    pub(super) async fn move_session(
        &self,
        id: Uuid,
        to: Option<&str>,
        effects: &[Effect],
        by: &By,
        problems: &mut Vec<String>,
    ) -> Result<(), DeskError> {
        let mut said: Vec<String> = Vec::new();
        // What it leaves behind, first.
        for e in effects {
            let done = match e {
                Effect::StaysUnassigned { ticket, .. } => {
                    let r = self.leave_ticket(*ticket, by).await;
                    if r.is_ok() {
                        said.push(
                            "Your ticket from there is back in To do, and no longer yours.".into(),
                        );
                    }
                    r
                }
                Effect::LeavesPlan { ticket, plan } => {
                    let r = self.0.store.leave_plan(*ticket).await.map_err(internal);
                    self.plan_touched(*plan).await;
                    r.map(drop)
                }
                Effect::AsksWithdrawn { .. } => {
                    let asks = self.asks_to_boss(id).await?;
                    if self.withdraw_asks(asks).await? {
                        said.push("Your questions to the boss you had are withdrawn.".into());
                    }
                    Ok(())
                }
                Effect::Release | Effect::ChargeTo { .. } => {
                    match self.charge_of(id).filter(Charge::live) {
                        Some(_) => self.release(id, by).await.map(drop),
                        None => Ok(()),
                    }
                }
                _ => Ok(()),
            };
            // What it leaves behind is left first: nothing is granted
            // while any of it stands.
            if let Err(e) = done {
                tracing::warn!(%id, error = %e, "a session's move stopped: what it leaves wasn't left");
                return Err(e);
            }
        }
        // The sidebar: from now on it is the new group's.
        self.0.scopes.lock().moving.insert(id);
        let key = session_item(id);
        let g = to.unwrap_or(UNGROUPED).to_string();
        let laid = self.change_layout(|l| l.put(&key, &g, None)).await;
        self.0.scopes.lock().moving.remove(&id);
        laid?;
        // What the new scope gains.
        let scope = self.scope_of_group(to);
        for e in effects {
            let done = match e {
                Effect::TicketGoes { ticket, board } => {
                    match self.ticket_goes(*ticket, *board, by).await {
                        Ok(key) => {
                            said.push(format!("Your ticket comes with you: it is {key} here."));
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
                Effect::ChargeTo { boss } => {
                    let party = Party::manager(boss, &self.named_boss(boss));
                    self.take_on(id, &party, How::Handed, &scope, by)
                        .await
                        .map(drop)
                }
                _ => Ok(()),
            };
            if let Err(e) = done {
                tracing::warn!(%id, error = %e, "part of a session's move wasn't done");
                problems.push(e.to_string());
            }
        }
        if effects.contains(&Effect::Tell) {
            self.tell_moved(id, to, &said).await;
        }
        Ok(())
    }

    /// Tells session `id` on its line that it moved to `to`, and `said`.
    pub(super) async fn tell_moved(&self, id: Uuid, to: Option<&str>, said: &[String]) {
        let Ok(view) = self.view(id) else {
            return;
        };
        let place = to
            .and_then(|g| self.group_name(g))
            .unwrap_or_else(|| "no group".into());
        let text = std::iter::once(format!(
            "You were moved to {place}, which another boss oversees."
        ))
        .chain(said.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
        let mut note = LineMessage::new(
            Party::desk(),
            Party::session(&view),
            LineKind::Tell,
            text,
            0,
        );
        note.group = to.map(str::to_string);
        if !view.live {
            note.delivery = Delivery::Held;
        }
        match self.0.store.put_line(&note).await {
            Ok(()) => {
                self.line_announce(&note);
                self.line_pump_later(id);
            }
            Err(e) => tracing::warn!(%id, error = %e, "a moved session wasn't told"),
        }
    }

    /// The boss `agent` leaves every room of `group`: that group's talk
    /// stays with it.
    async fn leave_rooms_of(&self, agent: &str, group: &str) -> Result<(), DeskError> {
        let me = format!("{}/{agent}", crate::rooms::LOCAL_HUB);
        let rooms: Vec<crate::rooms::Room> = self
            .0
            .store
            .rooms()
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|r| {
                r.group.as_deref() == Some(group) && r.members.iter().any(|m| m.key() == me)
            })
            .collect();
        for r in rooms {
            let members = r
                .members
                .iter()
                .filter(|m| m.key() != me)
                .cloned()
                .collect();
            self.update_room(
                r.id,
                crate::rooms::RoomPatch {
                    members: Some(members),
                    ..Default::default()
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Ticket `id` moves to `to`'s board, doing `effects`.
    async fn move_ticket_to(
        &self,
        id: Uuid,
        _to: Option<&str>,
        effects: &[Effect],
        by: &By,
        _problems: &mut Vec<String>,
    ) -> Result<(), DeskError> {
        for e in effects {
            let done = match e {
                Effect::Unassigned { ticket } => self.leave_ticket(*ticket, by).await,
                Effect::LeavesPlan { ticket, plan } => {
                    let r = self.0.store.leave_plan(*ticket).await.map_err(internal);
                    self.plan_touched(*plan).await;
                    r.map(drop)
                }
                _ => Ok(()),
            };
            if let Err(e) = done {
                tracing::warn!(%id, error = %e, "a ticket's move stopped: what it leaves wasn't left");
                return Err(e);
            }
        }
        for e in effects {
            if let Effect::TicketGoes { ticket, board } = e {
                self.ticket_goes(*ticket, *board, by).await?;
            }
        }
        Ok(())
    }

    /// The boss `agent` moves to oversee `to`, doing `effects`: the hub
    /// keeps its memory to the scope it leaves and writes its config; what
    /// it ran passes to the main boss, or back to the person.
    async fn move_boss_now(
        &self,
        agent: &str,
        to: Option<&str>,
        choices: &Choices,
        effects: &[Effect],
        by: &By,
        problems: &mut Vec<String>,
    ) -> Result<(), DeskError> {
        if effects.is_empty() {
            return Ok(());
        }
        let charges: Vec<Uuid> = self
            .charges()
            .iter()
            .filter(|c| c.boss == agent && c.released_at.is_none())
            .map(|c| c.session)
            .collect();
        let plans: Vec<Uuid> = self
            .0
            .store
            .plans(None)
            .await
            .map_err(internal)?
            .into_iter()
            .filter(|p| {
                p.boss == agent
                    && matches!(
                        p.state,
                        PlanState::Proposed | PlanState::Running | PlanState::Paused
                    )
            })
            .map(|p| p.id)
            .collect();
        let crew = self
            .crew()
            .ok_or_else(|| DeskError::Conflict("Desk can't move a boss on this hub.".into()))?;
        // Nothing of the old scope's runs on under it meanwhile.
        // Nothing of the old scope's runs on under it meanwhile; if any of
        // it can't be stopped, the boss isn't moved.
        for p in &plans {
            let running = self.0.store.plan(*p).await.map_err(internal)?;
            if running.is_some_and(|p| p.state == PlanState::Running) {
                self.pause_plan(*p, crate::plan::Paused::NoBoss, by).await?;
            }
        }
        for s in &charges {
            self.release(*s, by).await?;
        }
        // The boss it replaces lets go of what it ran, in this same move.
        for e in effects {
            if let Effect::Replaces { boss } = e {
                let theirs: Vec<Uuid> = self
                    .charges()
                    .iter()
                    .filter(|c| c.boss == *boss && c.released_at.is_none())
                    .map(|c| c.session)
                    .collect();
                for s in theirs {
                    self.release(s, by).await?;
                }
            }
        }
        // It leaves the rooms of the scope it leaves.
        let old = self
            .scope_of_boss(agent)
            .and_then(|s| s.group().map(str::to_string));
        if let Some(old) = old.as_deref() {
            self.leave_rooms_of(agent, old).await?;
        }
        crew.move_boss(agent, to, choices.replace)
            .await
            .map_err(DeskError::Conflict)?;
        let main = self
            .boss_of(&Scope::Ungrouped)
            .filter(|b| b.agent != agent)
            .map(|b| Party::manager(&b.agent, &b.name));
        let passes = effects
            .iter()
            .any(|e| matches!(e, Effect::PassesToMain { .. }));
        if let (true, Some(main)) = (passes, main) {
            for s in &charges {
                if let Err(e) = self.pass_charge(*s, &main, &Scope::Ungrouped, by).await {
                    problems.push(e.to_string());
                }
            }
            for p in &plans {
                if let Err(e) = self.pass_plan(*p, &main, by).await {
                    problems.push(e.to_string());
                }
            }
        }
        Ok(())
    }
}
