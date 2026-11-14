//! Charges ([`crate::charge`]) as the desk runs them: a boss takes a
//! session on (or the person hands it one), and lets it go; the person
//! takes the wheel by giving the session input, and hands it back. Each
//! change is stored, recorded in Activity and announced as
//! `{type: "charge", charge}`.
//!
//! While the person has the wheel the boss's messages to the session wait
//! on its line ([`crate::line::Wait::PersonDriving`]); who has it is read
//! from the person's last input each time it is asked, so a hold starts the
//! moment they type, and ends the hold after their last input
//! ([`charge::HOLD`], or what its boss's settings say). A loop of its own writes each change of hands down and
//! lets what waited go once the wheel is back.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::charge::{self, Charge, How, NotChargeable, Wheel};
use crate::crew::{self, By, Kind, Party, PartyKind};
use crate::line::{Delivery, LineKind, LineMessage};
use crate::model::{DeskEvent, SessionState, SessionView};
use crate::scope::Scope;

/// What the desk keeps of charges in memory: every row, and when the
/// person last gave each session input Desk doesn't see in its terminal.
#[derive(Default)]
pub(crate) struct Charges {
    rows: HashMap<Uuid, Charge>,
    /// The person's own prompts and composer messages, by session.
    driven: HashMap<Uuid, DateTime<Utc>>,
    /// What each boss that took a session on is called, by agent id.
    names: HashMap<String, String>,
    /// How long the person's input holds each boss's sessions, by agent
    /// id, as its settings say; [`charge::HOLD`] for one not said.
    holds: HashMap<String, chrono::Duration>,
    /// The repositories each boss acting fully may ship in, by agent id:
    /// only those whose Act fully is in force (named repositories, a
    /// budget not yet spent), as the hub says.
    full: HashMap<String, charge::FullGrant>,
    /// Each boss's Act fully budget and today's real money against it, by
    /// agent id, as the hub last read it: only bosses that chose Act fully
    /// with a budget.
    full_budgets: HashMap<String, crate::floor::FullBudget>,
    /// Every agent's own settings files (its `config.toml` and the like),
    /// as the hub says: withheld from what a boss answers for, as Desk's
    /// data is, so no boss allows a change to its own limits.
    agent_settings: Vec<std::path::PathBuf>,
    /// Why each boss that won't answer what its sessions ask won't, by
    /// agent id, as the hub says: it only watches or suggests, or Desk's
    /// breaker holds it. A boss left out answers.
    unanswering: HashMap<String, String>,
    /// The requests a boss was given and left open: the person's now.
    left: std::collections::HashSet<Uuid>,
    /// The sessions whose dialog questions an Act fully boss answers, and
    /// how long the person's input keeps them theirs, as the hub last said.
    dialogs: HashMap<Uuid, chrono::Duration>,
    /// Whether the rows were read. Until they are, Desk doesn't know which
    /// sessions a boss runs or who drives them: every boss's message to a
    /// session waits, and nothing is taken on.
    loaded: bool,
}

/// A charge as Desk shows it: its boss by name, and who has the wheel now.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargeView {
    pub session: Uuid,
    pub group: Option<String>,
    pub boss: String,
    pub boss_name: String,
    pub how: How,
    pub wheel: Wheel,
    pub wheel_since: Option<DateTime<Utc>>,
    /// How long the person's last input keeps the wheel theirs (unpinned),
    /// in minutes, as its boss's settings say.
    pub hold_minutes: i64,
    pub since: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
use crate::crew::UNNAMED_BOSS;

/// How often who has the wheel is looked at.
const SETTLE_EVERY: Duration = Duration::from_secs(5);

impl Desk {
    /// Reads the charges as kept; whether they were. What the person
    /// typed before a restart isn't known after it, so a wheel of theirs
    /// counts its hold from now.
    pub(super) async fn charge_load(&self) -> bool {
        match self.0.store.charges().await {
            Ok(rows) => {
                let now = crew::now();
                let rows = rows.into_iter().map(|mut c| {
                    if c.wheel == Wheel::Person {
                        c.wheel_since = Some(now);
                    }
                    (c.session, c)
                });
                let mut st = self.0.state.lock();
                st.charges.rows = rows.collect();
                st.charges.loaded = true;
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't read charges; bosses' messages wait until they are"
                );
                false
            }
        }
    }

    /// Whether Desk knows which sessions bosses run.
    pub fn charges_loaded(&self) -> bool {
        self.0.state.lock().charges.loaded
    }

    /// What the boss `id` is called: as the hub's roster says now, which
    /// outlives a restart, else as it was when it took a session on. Never
    /// its id: one with no name of its own is [`crate::crew::UNNAMED_BOSS`].
    pub(super) fn boss_name(&self, id: &str) -> String {
        let named = |n: &str| !n.trim().is_empty() && n != id;
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(crew) = crew {
            if let Some(n) = crew
                .overseer()
                .filter(|o| o.id == id && named(&o.name))
                .map(|o| o.name)
                .or_else(|| crew.agent_name(id).filter(|n| named(n)))
            {
                return n;
            }
        }
        let st = self.0.state.lock();
        crew::shown_name(id, st.charges.names.get(id).map(String::as_str), true)
    }

    /// How long the person's input holds the sessions of each boss, by its
    /// agent id, as the hub reads their settings: a boss left out holds
    /// for [`charge::HOLD`].
    pub fn set_holds(&self, holds: HashMap<String, chrono::Duration>) {
        self.0.state.lock().charges.holds = holds;
    }

    /// Why each boss that won't answer what the sessions it runs ask won't,
    /// by its agent id, as the hub reads their settings: a boss left out
    /// answers. The sessions whose badge this changes are shown again.
    pub fn set_unanswering(&self, why: HashMap<String, String>) {
        let changed = {
            let mut st = self.0.state.lock();
            let changed = st.charges.unanswering != why;
            st.charges.unanswering = why;
            changed
        };
        if changed {
            self.charges_waiting_shown();
        }
    }

    /// The request `approval` of `session` was given to its boss, whose
    /// turn left it open: the person's to answer now, as its badge says.
    pub fn leave_to_person(&self, session: Uuid, approval: Uuid) {
        // Shown again only when it is newly the person's: leaving it to them
        // again changes nothing anyone sees.
        let newly = {
            let mut st = self.0.state.lock();
            let open = st
                .live
                .get(&session)
                .and_then(|l| l.pending.as_ref())
                .is_some_and(|p| p.id == approval);
            open && {
                // Only what is still open anywhere is kept.
                let pending: std::collections::HashSet<Uuid> = st
                    .live
                    .values()
                    .filter_map(|l| l.pending.as_ref().map(|p| p.id))
                    .collect();
                st.charges.left.retain(|id| pending.contains(id));
                st.charges.left.insert(approval)
            }
        };
        if newly {
            if let Ok(view) = self.view(session) {
                self.announce(DeskEvent::Session {
                    session: Box::new(view),
                });
            }
        }
    }

    /// Shows again each session a boss runs that waits on a request, as
    /// who it waits on may have changed.
    fn charges_waiting_shown(&self) {
        let views: Vec<SessionView> = {
            let st = self.0.state.lock();
            st.charges
                .rows
                .values()
                .filter(|c| c.live())
                .filter(|c| st.live.get(&c.session).is_some_and(|l| l.pending.is_some()))
                .filter_map(|c| st.records.get(&c.session).map(|r| Self::view_of(&st, r)))
                .collect()
        };
        for view in views {
            self.announce(DeskEvent::Session {
                session: Box::new(view),
            });
        }
    }

    /// Why what `view` waits on is the person's to answer though a boss
    /// runs it, if it is: they drive it, only its terminal can answer it,
    /// its boss won't answer requests, or its boss left it to them. None
    /// while its boss has it, and when no boss runs it.
    pub(super) fn waits_on_you(st: &super::State, view: &SessionView) -> Option<String> {
        let p = view.pending.as_ref()?;
        let c = st.charges.rows.get(&view.id).filter(|c| c.live())?;
        let name = st
            .charges
            .names
            .get(&c.boss)
            .cloned()
            .unwrap_or_else(|| "Your boss".into());
        if Self::wheel_now(st, c).person() {
            return Some("You're driving it.".into());
        }
        if !p.answerable {
            return Some("Only its terminal can answer this.".into());
        }
        if let Some(why) = st.charges.unanswering.get(&c.boss) {
            return Some(why.clone());
        }
        if st.charges.left.contains(&p.id) {
            return Some(format!("{name} left this to you."));
        }
        None
    }

    /// The repositories each boss acting fully may ship in, by its agent
    /// id, as the hub reads their settings and spend: a boss left out
    /// doesn't act fully anywhere.
    pub fn set_full(&self, full: HashMap<String, charge::FullGrant>) {
        let changed = {
            let mut st = self.0.state.lock();
            let changed = st.charges.full != full;
            st.charges.full = full;
            changed
        };
        // A boss no longer unbound: its sessions go back to their own modes.
        if changed {
            self.modes_changed();
        }
    }

    /// Each boss's Act fully budget and today's real money spent against
    /// it, by agent id, as the hub reads them: the Floor and the boss's
    /// settings show it.
    pub fn set_full_budgets(&self, budgets: HashMap<String, crate::floor::FullBudget>) {
        self.0.state.lock().charges.full_budgets = budgets;
    }

    /// `boss`'s Act fully budget and today's real money, when it has one.
    pub fn full_budget_of(&self, boss: &str) -> Option<crate::floor::FullBudget> {
        self.0.state.lock().charges.full_budgets.get(boss).cloned()
    }

    /// Who judges where each boss served from a grant acts fully: the
    /// hub's crew. Without one, none does.
    pub fn served_full_with(&self, judge: std::sync::Arc<dyn charge::ServedFull>) {
        *self.0.served_full.lock() = Some(judge);
    }

    /// Where `boss` acts fully now; nowhere when it doesn't. A boss served
    /// from a grant is judged afresh, each time ([`charge::ServedFull`]).
    pub(crate) fn full_of(&self, boss: &str) -> charge::FullGrant {
        if crate::grant::is_served(boss) {
            let judge = self.0.served_full.lock().clone();
            return judge.and_then(|j| j.full_of(boss)).unwrap_or_default();
        }
        self.0
            .state
            .lock()
            .charges
            .full
            .get(boss)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `boss` acts fully in the repository session `id` works in:
    /// its own folder's, named in `[oversight.full] repos`.
    pub fn acts_fully_in(&self, id: Uuid, boss: &str) -> bool {
        let grant = self.full_of(boss);
        let Ok(view) = self.view(id) else {
            return false;
        };
        // Anywhere, every session's folder is its to ship in.
        if grant.anywhere {
            return true;
        }
        let named = grant.repos;
        if named.is_empty() {
            return false;
        }
        let home = self.0.boss_bounds.lock().as_ref().map(|b| b.home.clone());
        let Some(home) = home else {
            return false;
        };
        let own = charge::own_folder(std::path::Path::new(&view.cwd));
        repo_of_own(&own).is_some_and(|r| r.named(&named, &home))
    }

    /// The repository session `id` works in, as its `origin` names it
    /// (`owner/name`, lowercased): what a preference scoped to a
    /// repository covers. `None` when it works in none (or it isn't here).
    pub fn repo_of_session(&self, id: Uuid) -> Option<String> {
        let view = self.view(id).ok()?;
        let own = charge::own_folder(std::path::Path::new(&view.cwd));
        git_line_within(
            &own,
            &["remote", "get-url", "origin"],
            Duration::from_secs(2),
        )
        .and_then(|o| charge::repo_name(&o))
        .map(|r| r.path)
    }

    /// How long the person's input holds a session `boss` runs, as the hub
    /// last said.
    pub fn hold_for(&self, boss: &str) -> chrono::Duration {
        Self::hold_of(&self.0.state.lock(), boss)
    }

    /// How long the person's input holds a session `boss` runs.
    fn hold_of(st: &super::State, boss: &str) -> chrono::Duration {
        st.charges.holds.get(boss).copied().unwrap_or(charge::HOLD)
    }

    /// The person is writing to `id` in Desk's composer (a draft changed,
    /// not yet sent): it counts as their input, as a keystroke in its
    /// terminal does, so the boss doesn't answer underneath them. Desk's
    /// window says so at most every [`charge::TYPING_EVERY`] while they type.
    pub fn person_typing(&self, id: Uuid) -> Result<(), DeskError> {
        self.view(id)?;
        self.person_drove(id);
        Ok(())
    }

    /// The person gave `id` a prompt or wrote to it: they take its wheel.
    pub(super) fn person_drove(&self, id: Uuid) {
        let held = {
            let mut st = self.0.state.lock();
            st.charges.driven.insert(id, Utc::now());
            // A plan its boss approved is the person's to approve now.
            if let Some(l) = st.live.get_mut(&id) {
                l.plan_approved = None;
            }
            // Codex's question held for its boss is theirs now, in front or
            // waiting behind another ask.
            if let Some((_, q)) = st.live.get_mut(&id).and_then(|l| l.codex_question.as_mut()) {
                q.answerable = false;
            }
            st.live.get(&id).and_then(|l| {
                l.pending
                    .as_ref()
                    .filter(|p| {
                        p.answerable
                            && (l.holder().is_some_and(|h| h.question)
                                || l.codex_question.as_ref().is_some_and(|(_, q)| q.id == p.id))
                    })
                    .map(|p| p.id)
            })
        };
        // A question held for its boss is theirs now: its dialog opens.
        if let Some(p) = held {
            self.release_dialog(id, p);
        }
    }

    /// The questions sessions' dialogs ask that wait behind another ask:
    /// still open, though not what their sessions wait on now.
    pub fn questions_behind(&self) -> Vec<Uuid> {
        self.0
            .state
            .lock()
            .live
            .values()
            .filter_map(|l| l.open_question.as_ref().map(|(q, _)| q.id))
            .collect()
    }

    /// Lets session `id`'s question `pending`, held at its start for its
    /// boss, go to its dialog: the person answers it there.
    pub fn release_dialog(&self, id: Uuid, pending: Uuid) {
        let answer = {
            let mut st = self.0.state.lock();
            let Some(l) = st.live.get_mut(&id) else {
                return;
            };
            let Some(p) = l
                .pending
                .as_mut()
                .filter(|p| p.id == pending && p.answerable)
            else {
                return;
            };
            p.answerable = false;
            // Codex's question, kept for when what it asks in front ends.
            if let Some((_, q)) = l.codex_question.as_mut().filter(|(_, q)| q.id == pending) {
                q.answerable = false;
            }
            l.answer.take()
        };
        if let Some(a) = answer {
            let _ = a.send(crate::claude::ask_in_terminal());
        }
        self.change(id, |_, _| {});
    }

    /// When the person last gave session `id` input ([`Self::last_input`]).
    pub fn last_input_of(&self, id: Uuid) -> Option<DateTime<Utc>> {
        Self::last_input(&self.0.state.lock(), id)
    }

    /// When the person last gave `id` input: typed in its terminal,
    /// prompted it, or wrote to it.
    pub(super) fn last_input(st: &super::State, id: Uuid) -> Option<DateTime<Utc>> {
        let typed = st.live.get(&id).and_then(|l| l.pty.typed_at());
        typed.max(st.charges.driven.get(&id).copied())
    }

    /// `c`'s wheel now, as `st` reads.
    fn wheel_now(st: &super::State, c: &Charge) -> Wheel {
        charge::wheel(
            c,
            Self::last_input(st, c.session),
            Self::hold_of(st, &c.boss),
            Utc::now(),
        )
    }

    fn charge_view(&self, c: &Charge, wheel: Wheel) -> ChargeView {
        ChargeView {
            session: c.session,
            group: c.group.clone(),
            boss: c.boss.clone(),
            boss_name: self.boss_name(&c.boss),
            how: c.how,
            wheel,
            wheel_since: c.wheel_since,
            hold_minutes: self.hold_for(&c.boss).num_minutes(),
            since: c.since,
            released_at: c.released_at,
        }
    }

    /// `id`'s charge, released or not.
    pub fn charge_of(&self, id: Uuid) -> Option<Charge> {
        self.0.state.lock().charges.rows.get(&id).cloned()
    }

    /// Every live charge, with its boss's name and who has the wheel.
    pub fn charges(&self) -> Vec<ChargeView> {
        let live: Vec<(Charge, Wheel)> = {
            let st = self.0.state.lock();
            st.charges
                .rows
                .values()
                .filter(|c| c.live())
                .map(|c| (c.clone(), Self::wheel_now(&st, c)))
                .collect()
        };
        let mut views: Vec<ChargeView> =
            live.iter().map(|(c, w)| self.charge_view(c, *w)).collect();
        views.sort_by_key(|v| v.since);
        views
    }

    /// Who drives `id` now, while a boss runs it.
    pub fn wheel_of(&self, id: Uuid) -> Option<Wheel> {
        let st = self.0.state.lock();
        let c = st.charges.rows.get(&id).filter(|c| c.live())?;
        Some(Self::wheel_now(&st, c))
    }

    /// Whether the person is in control of `id` for a question it asks:
    /// they gave it input (typed in its terminal or Desk's composer,
    /// prompted it, wrote to it) less than `gap` ago, or they pinned its
    /// wheel. A session a boss runs counts only input since the boss last
    /// had its wheel, as its wheel does; one it only oversees, any. The
    /// boss stays silent on its questions while this holds. `gap` is never
    /// shorter than the hold (the hub's `answer_gap` makes sure), so an answer is
    /// never queued behind the person's wheel.
    pub fn person_in_control(&self, id: Uuid, gap: chrono::Duration) -> bool {
        let st = self.0.state.lock();
        let input = Self::last_input(&st, id);
        match st.charges.rows.get(&id).filter(|c| c.live()) {
            Some(c) => charge::wheel(c, input, gap, Utc::now()).person(),
            None => input.is_some_and(|t| Utc::now() - t < gap),
        }
    }

    /// The sessions whose dialog questions an Act fully boss answers itself,
    /// and how long the person's input there keeps them theirs: said by the
    /// hub as it sweeps.
    pub fn set_dialog_answerers(&self, dialogs: HashMap<Uuid, chrono::Duration>) {
        self.0.state.lock().charges.dialogs = dialogs;
    }

    /// Whether `id`'s next dialog question is held for its boss to answer:
    /// an Act fully boss answers its questions, and the person isn't in
    /// control of it.
    pub(super) fn holds_dialog_for_boss(&self, id: Uuid) -> bool {
        let gap = self.0.state.lock().charges.dialogs.get(&id).copied();
        gap.is_some_and(|gap| !self.person_in_control(id, gap))
    }

    /// Whether what `id` waits on is a question its dialog asks the person
    /// (Claude's AskUserQuestion or plan, Codex's question or an MCP
    /// server's), not a permission request.
    pub fn asks_in_dialog(&self, id: Uuid) -> bool {
        let st = self.0.state.lock();
        let Some(l) = st.live.get(&id) else {
            return false;
        };
        let Some(p) = l.pending.as_ref() else {
            return false;
        };
        l.holder().is_some_and(|h| h.question)
            || l.open_question.as_ref().is_some_and(|(q, _)| q.id == p.id)
            || l.codex_question.as_ref().is_some_and(|(_, q)| q.id == p.id)
    }

    /// Whether the person has the wheel of `id`, which a boss runs: its
    /// boss's messages to it wait.
    pub(crate) fn person_driving(&self, id: Uuid) -> bool {
        // Not knowing who drives, it is held.
        !self.charges_loaded() || self.wheel_of(id).is_some_and(Wheel::person)
    }

    /// Where a boss's answers for its sessions are judged, as the hub
    /// running it says.
    /// Desk's own run folder is always withheld: its sessions' sockets and
    /// token pipes, and each Devin session's hook token and config copy,
    /// which a boss writing could answer for Desk or launder settings
    /// through.
    pub fn boss_bounds_with(&self, mut bounds: charge::Bounds) {
        let run = &self.0.opts.run_dir;
        bounds
            .withheld
            .push(crate::computer::boss::resolve_real(run).unwrap_or_else(|| run.clone()));
        *self.0.boss_bounds.lock() = Some(bounds);
    }

    /// Every agent's own settings files, as the hub serves them now: no
    /// boss allows a session to read or change them, wherever they are.
    pub fn withhold_agent_settings(&self, paths: Vec<std::path::PathBuf>) {
        self.0.state.lock().charges.agent_settings = paths;
    }

    /// Whether `boss` may answer `id`'s open request `approval` itself; why
    /// not, in the person's words, when it may not. Where it acts fully,
    /// what ships is its to allow too.
    pub fn boss_answerable(
        &self,
        id: Uuid,
        approval: Uuid,
        boss: &str,
    ) -> Result<(), charge::Irreversible> {
        let Some(pending) = self.pending_of(id, approval) else {
            return Err(charge::Irreversible("already answered".into()));
        };
        self.boss_judges(id, &pending, boss)
    }

    /// Whether `id` may be run by a boss served from a grant: it isn't
    /// running, or its CLI was launched with the pre-tool hook that fails
    /// closed. One already running with the person's hook (which lets a
    /// call go on when Desk can't answer) is restarted for it first.
    pub fn servable_launch(&self, id: Uuid) -> bool {
        self.0
            .state
            .lock()
            .live
            .get(&id)
            .is_none_or(|l| l.fail_closed)
    }

    /// Whether a boss served from a grant runs `id` now.
    pub fn served_runs(&self, id: Uuid) -> bool {
        self.charge_of(id)
            .filter(Charge::live)
            .is_some_and(|c| crate::grant::is_served(&c.boss))
    }

    /// What becomes of a tool call of `id`, a session a boss served from a
    /// grant runs, before it runs: none when Desk judges it safe, as it
    /// would for that boss's answer to it ([`Self::boss_judges`]). Every
    /// call is judged so, sub-agents' too, whatever the person's own allow
    /// rules say: their rules are for the person, not for the boss. One
    /// Desk doesn't judge safe is refused, or, while that boss is at Act
    /// fully in force, asked, so its boss decides it as any request
    /// (never allowed by this).
    pub fn served_refuses(
        &self,
        id: Uuid,
        tool: &str,
        input: &serde_json::Value,
        cwd: Option<String>,
        mode: Option<crate::mode::Mode>,
    ) -> Option<ServedCheck> {
        let boss = self.charge_of(id).filter(Charge::live)?.boss;
        if !crate::grant::is_served(&boss) {
            return None;
        }
        let pending = crate::model::PendingApproval {
            id: Uuid::nil(),
            tool: tool.to_string(),
            detail: String::new(),
            since: Utc::now(),
            answerable: false,
            choices: Vec::new(),
            why: None,
            options: None,
            request: Some(crate::model::Request {
                tool: tool.to_string(),
                input: input.clone(),
                cwd,
                read_only: false,
            }),
            call: None,
            agent: None,
            cli_session: None,
        };
        let why = self.boss_judges(id, &pending, &boss).err()?;
        Some(match self.served_asks(id, &boss, mode) {
            true => ServedCheck::Ask(format!(
                "A boss on another hub runs this session, and this call {}: its boss decides it.",
                why.0
            )),
            false => ServedCheck::Deny(format!(
                "A boss on another hub runs this session, and this call {}: it needs the person, \
                 in Desk.",
                why.0
            )),
        })
    }
}

impl Desk {
    /// Whether a call of `id` Desk doesn't judge safe is asked, for `boss`
    /// (served from a grant) to decide: while it is at Act fully in force,
    /// a live grant of it holds the session, and the session's mode, as
    /// this call's own hook says it (`mode`), is one where an asked call
    /// waits for an answer. In a mode that asks nothing (bypass, accept
    /// edits), or one not said, an asked call might run unasked, so there
    /// it is refused.
    fn served_asks(&self, id: Uuid, boss: &str, mode: Option<crate::mode::Mode>) -> bool {
        use crate::mode::Mode;
        matches!(mode, Some(Mode::Default | Mode::Plan | Mode::Auto))
            && self.full_of(boss).unbound
            && self
                .0
                .served_full
                .lock()
                .clone()
                .is_some_and(|j| j.holds(boss, id))
    }
}

/// What a served session's tool call Desk doesn't judge safe meets
/// ([`Desk::served_refuses`]), with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedCheck {
    /// Refused before it runs.
    Deny(String),
    /// Asked, as a request its boss decides.
    Ask(String),
}

impl Desk {
    /// Whether `boss` may let `pending`, a request of `id`, go on itself:
    /// what Desk reads in full and judges safe.
    fn boss_judges(
        &self,
        id: Uuid,
        pending: &crate::model::PendingApproval,
        boss: &str,
    ) -> Result<(), charge::Irreversible> {
        let no = |why: &str| Err(charge::Irreversible(why.into()));
        let Some(mut bounds) = self.0.boss_bounds.lock().clone() else {
            return no("Desk can't judge requests for a boss yet");
        };
        let settings = self.0.state.lock().charges.agent_settings.clone();
        bounds.withheld.extend(settings);
        let Ok(view) = self.view(id) else {
            return no("that session isn't there any more");
        };
        let moved = self.0.state.lock().live.get(&id).is_some_and(|l| l.moved);
        let mut facts = charge::facts_of(view.engine, pending, &view, &bounds.home, moved);
        // Its folder as the disk spells it.
        if let Ok(own) = std::fs::canonicalize(&facts.own) {
            facts.own = own;
        }
        // The repository the request runs in says which branch it is on
        // and which is its default, now: not what Desk last saw. Only the
        // own folder's repository counts: a push from another one (a
        // nested repository, a submodule, a cwd elsewhere) has no branch
        // the boss may push.
        if let Some(cwd) = facts.cwd.clone() {
            let top = git_line(&cwd, &["rev-parse", "--show-toplevel"])
                .and_then(|t| crate::computer::boss::resolve_real(std::path::Path::new(&t)));
            let own = crate::computer::boss::resolve_real(&facts.own);
            if top.is_some() && top == own {
                facts.branch = git_line(&cwd, &["symbolic-ref", "--short", "-q", "HEAD"]);
                facts.default_branch = git_line(
                    &cwd,
                    &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
                )
                .map(|d| d.strip_prefix("origin/").map(str::to_string).unwrap_or(d));
                if let Some(default) = facts.default_branch.clone() {
                    let tip = format!("refs/remotes/origin/{default}");
                    let head = git_line(&cwd, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]);
                    let at = git_line(
                        &cwd,
                        &["rev-parse", "-q", "--verify", &format!("{tip}^{{commit}}")],
                    );
                    facts.at_tip = head.is_some() && head == at;
                    facts.tip_tags = git_lines(&cwd, &["tag", "--points-at", &tip]);
                    facts.unpushed =
                        git_line(&cwd, &["rev-list", "--count", &format!("{tip}..HEAD")])
                            .is_none_or(|n| n != "0");
                }
                facts.branches = git_lines(
                    &cwd,
                    &[
                        "for-each-ref",
                        "--format=%(refname:short)",
                        "refs/heads",
                        "refs/remotes",
                    ],
                )
                .into_iter()
                .map(|b| {
                    b.split_once('/')
                        .filter(|(r, _)| *r == "origin")
                        .map_or(b.clone(), |(_, n)| n.to_string())
                })
                .collect();
                let grant = self.full_of(boss);
                let named = grant.repos;
                facts.workflows = grant.workflows;
                facts.any_workflow = grant.anywhere;
                if let Some(repo) = repo_of_own(&facts.own) {
                    facts.follow_tags = repo.follow_tags;
                    facts.hooks = repo.hooks;
                    facts.ships =
                        grant.anywhere || (!named.is_empty() && repo.named(&named, &bounds.home));
                    // gh acts on what its remotes and `set-default` say, or
                    // on GH_REPO and GH_HOST in the environment sessions
                    // start with (Desk's own); set in a command, the
                    // allowlist refuses them.
                    let gh_env = ["GH_REPO", "GH_HOST"]
                        .iter()
                        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
                    // Anywhere, whichever repository `gh` resolves is one
                    // it may ship in: the checks of the named one fall away.
                    facts.ships_gh = facts.ships
                        && (grant.anywhere
                            || (!gh_env
                                && charge::remotes_one_repo(
                                    repo.origin.as_deref(),
                                    &repo.remotes,
                                )
                                && charge::gh_resolved_named(
                                    repo.origin.as_deref(),
                                    &repo.gh_resolved,
                                )));
                    // What `origin/<default>` says is only as fresh as the
                    // last fetch: `origin` itself must say the same, now,
                    // before a tag or a push is judged by it.
                    if facts.ships {
                        let fresh = facts.default_branch.as_deref().is_some_and(|d| {
                            let remote = git_line_within(
                                &cwd,
                                &["ls-remote", "origin", &format!("refs/heads/{d}")],
                                LS_REMOTE_WITHIN,
                            )
                            .and_then(|l| l.split_whitespace().next().map(str::to_string));
                            let local = git_line(
                                &cwd,
                                &[
                                    "rev-parse",
                                    "-q",
                                    "--verify",
                                    &format!("refs/remotes/origin/{d}"),
                                ],
                            );
                            remote.is_some() && remote == local
                        });
                        if !fresh {
                            facts.at_tip = false;
                            facts.tip_tags.clear();
                            facts.unpushed = true;
                        }
                    }
                }
            }
        }
        let top = crate::computer::boss::resolve_real(&facts.own);
        let tracked = |p: &std::path::Path| {
            top.as_ref().is_some_and(|top| {
                p.strip_prefix(top).is_ok_and(|rel| {
                    std::process::Command::new("git")
                        .arg("-C")
                        .arg(top)
                        .args(["ls-files", "--error-unmatch", "--"])
                        .arg(rel)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                })
            })
        };
        charge::answerable(
            &facts,
            &bounds,
            &crate::computer::boss::resolve_real,
            &tracked,
        )
    }

    /// Whether `id`'s open request `approval` would change an agent's own
    /// settings (its config, where its oversight, budget and limits are),
    /// as far as Desk can tell: a file it writes, or a command that may
    /// write one ([`command_may_touch_settings`]). Reading them is
    /// allowed. Not knowing where home is yet, everything counts.
    fn touches_agent_settings(&self, id: Uuid, approval: Uuid) -> bool {
        let settings = self.0.state.lock().charges.agent_settings.clone();
        if settings.is_empty() {
            return false;
        }
        let Some(pending) = self.pending_of(id, approval) else {
            return false;
        };
        // Not knowing where home is, what it reads can't be told: held, as
        // Desk allows everything else at once (`allow`).
        let Some(home) = self.0.boss_bounds.lock().as_ref().map(|b| b.home.clone()) else {
            return true;
        };
        let Ok(view) = self.view(id) else {
            return false;
        };
        let facts = charge::facts_of(view.engine, &pending, &view, &home, false);
        let named = |p: &std::path::Path| {
            let p = crate::computer::boss::resolve_real(p).unwrap_or_else(|| p.to_path_buf());
            settings.iter().any(|s| p.starts_with(s))
        };
        match &facts.what {
            // Reading them changes nothing: only a write is the person's.
            charge::What::Change(paths) => paths.iter().any(|p| named(p)),
            charge::What::Read(_) | charge::What::Web { .. } | charge::What::Inside => false,
            charge::What::Command(command) => command_may_touch_settings(
                command,
                std::path::Path::new(&view.cwd),
                &settings,
                &home,
            ),
            // What Desk doesn't read (an MCP server's tool): held when its
            // input names a workspace, as it may write there. Its input, as
            // text, is no program Desk knows, so it counts as a write.
            _ => pending.request.as_ref().is_some_and(|r| {
                command_may_touch_settings(
                    &r.input.to_string(),
                    std::path::Path::new(&view.cwd),
                    &settings,
                    &home,
                )
            }),
        }
    }

    /// Whether `by`, when it is a boss, may act on `id` now: a session a
    /// boss runs is acted on only by that boss, and only while it has the
    /// wheel, unless that boss is unbound (Act fully), whom the person
    /// driving doesn't hold. Anyone else, and a session no boss runs, is
    /// the hub's rule.
    pub(crate) fn boss_holds(&self, id: Uuid, by: Option<&By>) -> Result<(), DeskError> {
        let Some(boss) = by.filter(|b| b.party.kind == PartyKind::Manager) else {
            return Ok(());
        };
        let Some(c) = self.charge_of(id).filter(Charge::live) else {
            return Ok(());
        };
        let name = self
            .view(id)
            .map(|v| v.label().to_string())
            .unwrap_or_else(|_| "that session".into());
        if c.boss != boss.party.id {
            return Err(DeskError::Conflict(format!(
                "another boss runs {name}; it's theirs or the person's to act on"
            )));
        }
        if self.wheel_of(id).is_some_and(Wheel::person) && !self.full_of(&boss.party.id).unbound {
            return Err(DeskError::Conflict(format!(
                "the person is driving {name}, so it's theirs to act on now"
            )));
        }
        Ok(())
    }

    /// Whether `boss` is unbound now, as the hub last said: at Act fully,
    /// in force. Desk allows what the sessions it oversees ask at once.
    pub fn boss_unbound(&self, boss: &str) -> bool {
        self.full_of(boss).unbound
    }

    /// Whether Desk knows where home is yet: until it does, an unbound
    /// boss allows nothing, as what a request reaches can't be told.
    pub fn boss_home_known(&self) -> bool {
        self.0.boss_bounds.lock().is_some()
    }

    /// The bosses unbound now.
    pub fn unbound_bosses(&self) -> Vec<String> {
        let st = self.0.state.lock();
        st.charges
            .full
            .iter()
            .filter(|(_, g)| g.unbound)
            .map(|(b, _)| b.clone())
            .collect()
    }

    /// Whether any boss is unbound now: else no request is Desk's to allow
    /// at once, and nothing need be looked at.
    pub fn any_boss_unbound(&self) -> bool {
        if self.0.state.lock().charges.full.values().any(|g| g.unbound) {
            return true;
        }
        // A boss served from a grant, judged now, for each that runs a
        // session here.
        let served: HashSet<String> = self
            .0
            .state
            .lock()
            .charges
            .rows
            .values()
            .filter(|c| c.live() && crate::grant::is_served(&c.boss))
            .map(|c| c.boss.clone())
            .collect();
        served.into_iter().any(|b| self.full_of(&b).unbound)
    }

    /// `id`'s open request `approval`: the one it waits on, or another of
    /// Codex's.
    fn pending_of(&self, id: Uuid, approval: Uuid) -> Option<crate::model::PendingApproval> {
        let st = self.0.state.lock();
        let l = st.live.get(&id)?;
        l.pending
            .iter()
            .chain(l.codex_asks.iter().map(|a| &a.pending))
            .find(|p| p.id == approval)
            .cloned()
    }

    /// Whether `boss` may make `decision` on `id`'s request `approval`:
    /// on a session it runs, only while it has the wheel and Desk's breaker
    /// doesn't hold it, never for the rest of the session, and to allow
    /// only what [`charge::answerable`] says it may. A session no boss runs
    /// is the hub's rule to judge.
    pub(super) fn boss_decides(
        &self,
        id: Uuid,
        approval: Uuid,
        decision: crate::model::Decision,
        boss: &By,
    ) -> Result<(), DeskError> {
        use crate::model::Decision;
        // Not knowing yet which sessions bosses run, no boss answers.
        if !self.charges_loaded() {
            return Err(DeskError::Conflict(
                "Desk can't read which sessions bosses run just now; it's the person's to answer"
                    .into(),
            ));
        }
        // Unbound, it allows what it is asked, on any session, but never
        // what reads or changes an agent's own settings: its budget and
        // limits stay the person's.
        let unbound = self.full_of(&boss.party.id).unbound;
        if unbound && decision == Decision::Allow {
            // Held by its Desk budget or a breaker of its own, it takes no
            // turn, and allows nothing either.
            if let Some(why) = self.agent_held_why(&boss.party.id) {
                return Err(DeskError::Conflict(format!(
                    "{} is {why}: it's the person's to allow until they let it go on",
                    boss.party.name
                )));
            }
            if self.0.boss_bounds.lock().is_none() {
                return Err(DeskError::Conflict(
                    "Desk doesn't know yet where home is, so it can't tell what it reaches".into(),
                ));
            }
            if self.touches_agent_settings(id, approval) {
                return Err(DeskError::Conflict(
                    "it may read or change an agent's own settings, where its budget and limits \
                     are: it's the person's to allow"
                        .into(),
                ));
            }
        }
        // A session Desk's breaker paused goes on only at the person's
        // word, whoever runs it.
        self.breaker_refuses_prompt(id, Some(boss))?;
        if !self.charge_of(id).is_some_and(|c| c.live()) {
            return Ok(());
        }
        self.boss_holds(id, Some(boss))?;
        // Allowing what it asks drives it; denying makes nothing happen.
        if decision == Decision::Allow {
            self.mode_lets(id, Some(boss))?;
        }
        if self.pending_of(id, approval).is_none() {
            return Err(DeskError::Conflict("already answered".into()));
        }
        if unbound && decision == Decision::Allow {
            return Ok(());
        }
        match decision {
            Decision::Deny | Decision::Cancel => Ok(()),
            Decision::Allow => self
                .boss_answerable(id, approval, &boss.party.id)
                .map_err(|why| DeskError::Conflict(format!("{why}: it's the person's to allow"))),
            Decision::AllowForSession | Decision::Terminal => Err(DeskError::Invalid(
                "only the person answers for the rest of a session".into(),
            )),
        }
    }

    /// How many of `boss`'s sessions are starting or working: the slots
    /// they hold.
    pub fn charges_busy(&self, boss: &str) -> usize {
        let st = self.0.state.lock();
        let mine: Vec<Charge> = st
            .charges
            .rows
            .values()
            .filter(|c| c.boss == boss)
            .cloned()
            .collect();
        let states: HashMap<Uuid, SessionState> = mine
            .iter()
            .filter_map(|c| st.records.get(&c.session).map(|r| (c.session, r.state)))
            .collect();
        charge::busy(&mine, &states)
    }

    async fn put_charge(&self, c: Charge, wheel: Wheel) -> Result<ChargeView, DeskError> {
        self.0.store.put_charge(&c).await?;
        self.0
            .state
            .lock()
            .charges
            .rows
            .insert(c.session, c.clone());
        // Released, or another boss's: a mode the last one kept it in ends.
        self.modes_changed();
        let view = self.charge_view(&c, wheel);
        let _ = self.0.events.send(DeskEvent::Charge {
            charge: Box::new(view.clone()),
        });
        // Who what it waits on waits on changes with who runs and drives it.
        if let Ok(now) = self.view(c.session) {
            if now.pending.is_some() {
                self.announce(DeskEvent::Session {
                    session: Box::new(now),
                });
            }
        }
        Ok(view)
    }

    /// Notes on `id`'s line, in its thread, `text` from `boss`: never typed
    /// in, so it starts no turn (a session just started, or one the person
    /// now runs alone, does nothing because of it). What goes wrong is only
    /// logged: the charge stands either way.
    async fn charge_note(&self, view: &crate::model::SessionView, boss: &Party, text: &str) {
        let mut m = LineMessage::new(
            boss.clone(),
            Party::session(view),
            LineKind::Tell,
            text.to_string(),
            1,
        );
        m.delivery = Delivery::Noted;
        if let Err(e) = self.0.store.put_line(&m).await {
            tracing::info!(session = %view.id, error = %e, "a session's charge wasn't noted on its line");
            return;
        }
        let _ = self.0.events.send(DeskEvent::Line {
            message: Box::new(m),
            removed: false,
        });
    }

    /// What `id`'s boss said to it that hasn't gone in never will: it no
    /// longer runs the session. Each is marked withdrawn in the thread.
    async fn charge_withdraw(&self, id: Uuid) {
        match self.0.store.line_withdraw_overseers(&id.to_string()).await {
            Ok(rows) => {
                for m in rows {
                    let _ = self.0.events.send(DeskEvent::Line {
                        message: Box::new(m),
                        removed: false,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't withdraw a boss's messages")
            }
        }
    }

    /// `boss`, the boss of `scope`, takes `id` on (`how`), as `by`: the
    /// boss itself, or the person handing it over. Only a session in its
    /// scope can be; the charge is that scope's. Taking on one already the
    /// boss's changes nothing.
    pub async fn take_on(
        &self,
        id: Uuid,
        boss: &Party,
        how: How,
        scope: &Scope,
        by: &By,
    ) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        if !self.charges_loaded() {
            return Err(DeskError::Conflict(
                "Desk can't read which sessions bosses run just now; try again shortly.".into(),
            ));
        }
        charge::chargeable(&view, scope, &self.bossed())
            .map_err(|e: NotChargeable| DeskError::Conflict(e.to_string()))?;
        // Only on an engine whose every call Desk checks and refuses when it
        // can't: a stopped Codex session would come back without it.
        if crate::grant::is_served(&boss.id) && !crate::engine::boss_safe(view.engine) {
            return Err(DeskError::Conflict(format!(
                "A boss on another hub runs only sessions Desk checks every call of, so not {}.",
                view.label()
            )));
        }
        if crate::grant::is_served(&boss.id) && !self.servable_launch(id) {
            return Err(DeskError::Conflict(format!(
                "{} runs without Desk's closed check of each call, so a boss on another hub can't run it; stop it, then hand it over.",
                view.label()
            )));
        }
        let existing = self.charge_of(id).filter(Charge::live);
        if let Some(c) = existing.as_ref().filter(|c| c.boss == boss.id) {
            let wheel = self.wheel_of(id).unwrap_or(Wheel::Boss);
            return Ok(self.charge_view(c, wheel));
        }
        self.0
            .state
            .lock()
            .charges
            .names
            .insert(boss.id.clone(), boss.name.clone());
        let mut c = Charge::new(id, &boss.id, how, scope.group().map(str::to_string));
        // From another boss: the person driving it keeps driving.
        let wheel = match existing {
            Some(_) => self.wheel_of(id).unwrap_or(Wheel::Boss),
            None => Wheel::Boss,
        };
        if wheel.person() {
            c.wheel = wheel;
            c.wheel_since = Some(c.since);
        }
        let shown = self.put_charge(c, wheel).await?;
        let name = view.label().to_string();
        let text = match by.party.kind {
            PartyKind::You => format!("handed {name} to {}", boss.name),
            _ => format!("took on {name}"),
        };
        self.note(by.event(Kind::TakenOn, Some(Party::session(&view)), text));
        self.charge_note(
            &view,
            boss,
            "I'm running this session with the person now. Ask me (to: \"desk\") when you're unsure; they can take over at any time.",
        )
        .await;
        Ok(shown)
    }

    /// `id`, a session about to start, is `boss`'s from the start, in
    /// `group`: the boss started it.
    pub(super) async fn charge_started(
        &self,
        id: Uuid,
        boss: &Party,
        group: Option<&str>,
    ) -> Result<(), DeskError> {
        self.0
            .state
            .lock()
            .charges
            .names
            .insert(boss.id.clone(), boss.name.clone());
        let c = Charge::new(id, &boss.id, How::Started, group.map(str::to_string));
        self.put_charge(c, Wheel::Boss).await.map(drop)
    }

    /// Makes `id`, a session about to start, `boss`'s in `st`, under the
    /// same lock its record goes in with: no one ever sees it there and
    /// unrun, so a boss that takes on every session never takes this one
    /// on as it starts. One that fails to start is forgotten with it.
    pub(super) fn reserve_charge(
        st: &mut super::State,
        id: Uuid,
        boss: &Party,
        group: Option<&str>,
    ) {
        st.charges.names.insert(boss.id.clone(), boss.name.clone());
        let c = Charge::new(id, &boss.id, How::Started, group.map(str::to_string));
        st.charges.rows.insert(id, c);
    }

    /// Forgets `id`'s charge: it never ran. Desk is told it is let go, as
    /// for any charge that ends, so none is left showing.
    pub(super) fn charge_forget(&self, id: Uuid) {
        let Some(mut c) = self.0.state.lock().charges.rows.remove(&id) else {
            return;
        };
        let now = crew::now();
        c.released_at = Some(now);
        c.updated_at = now;
        let view = self.charge_view(&c, Wheel::Boss);
        let _ = self.0.events.send(DeskEvent::Charge {
            charge: Box::new(view),
        });
    }

    /// Notes in `view`'s thread that `boss`, which started it, runs it.
    pub(super) async fn started_note(&self, view: &crate::model::SessionView, boss: &Party) {
        self.charge_note(
            view,
            boss,
            "I started this session for the person and I'm running it with them. Ask me (to: \"desk\") when you're unsure; they can take over at any time.",
        )
        .await;
    }

    /// `id`, which a boss no longer overseeing ran, passes to `boss`; or,
    /// when it can't be run any more (it ended), is let go.
    pub async fn pass_charge(
        &self,
        id: Uuid,
        boss: &Party,
        scope: &Scope,
        by: &By,
    ) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        match charge::chargeable(&view, scope, &self.bossed()) {
            Ok(()) => {
                let how = self.charge_of(id).map_or(How::Taken, |c| c.how);
                self.take_on(id, boss, how, scope, by).await
            }
            Err(_) => self.release(id, by).await,
        }
    }

    /// `id`'s boss lets it go, or the person takes it back (`by`). Its row
    /// stays, so a boss that takes on every session never takes it again.
    pub async fn release(&self, id: Uuid, by: &By) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        let Some(mut c) = self.charge_of(id).filter(Charge::live) else {
            return Err(DeskError::Conflict("No boss runs that session.".into()));
        };
        // What it said and the person held goes nowhere: before the release
        // lets it through, and again for anything said meanwhile.
        self.charge_withdraw(id).await;
        let now = crew::now();
        c.released_at = Some(now);
        c.updated_at = now;
        let boss = Party::manager(&c.boss, &self.boss_name(&c.boss));
        let served = crate::grant::is_served(&c.boss);
        let wheel = c.wheel;
        let shown = self.put_charge(c, wheel).await?;
        self.charge_withdraw(id).await;
        // A boss served from a grant let go of it: the turn it set going
        // ends here, so nothing of its work goes on under the person's own
        // rules. The person starts it again if they want it.
        if served
            && matches!(
                view.state,
                crate::model::SessionState::Working
                    | crate::model::SessionState::Blocked
                    | crate::model::SessionState::Asking
            )
        {
            if let Err(e) = self.interrupt(id, Some(By::desk("Its boss no longer runs it."))) {
                tracing::info!(%id, error = %e, "a session let go by its served boss wasn't interrupted");
            }
        }
        let name = view.label().to_string();
        let text = match by.party.kind {
            PartyKind::You => format!("took {name} back from {}", boss.name),
            _ => format!("let {name} go"),
        };
        self.note(by.event(Kind::Released, Some(Party::session(&view)), text));
        self.charge_note(
            &view,
            &boss,
            "I'm no longer running this session: the person runs it on their own now.",
        )
        .await;
        Ok(shown)
    }

    /// The person takes the wheel of `id` until they hand it back.
    pub async fn drive(&self, id: Uuid) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        let Some(mut c) = self.charge_of(id).filter(Charge::live) else {
            return Err(DeskError::Conflict("No boss runs that session.".into()));
        };
        let now = crew::now();
        c.wheel = Wheel::PersonPinned;
        c.wheel_since = Some(now);
        c.updated_at = now;
        let shown = self.put_charge(c, Wheel::PersonPinned).await?;
        self.note(By::you().event(
            Kind::Driving,
            Some(Party::session(&view)),
            format!("is driving {} until they hand it back", view.label()),
        ));
        Ok(shown)
    }

    /// The person hands `id` back to its boss: what the boss said to it
    /// while they drove goes in now.
    pub async fn hand_back(&self, id: Uuid) -> Result<ChargeView, DeskError> {
        let view = self.view(id)?;
        let Some(c) = self.charge_of(id).filter(Charge::live) else {
            return Err(DeskError::Conflict("No boss runs that session.".into()));
        };
        let boss = self.boss_name(&c.boss);
        let shown = self.wheel_back(c).await?;
        self.note(By::you().event(
            Kind::HandedBack,
            Some(Party::session(&view)),
            format!("handed {} back to {boss}", view.label()),
        ));
        Ok(shown)
    }

    /// The boss has `c`'s wheel again, from now: input before doesn't take
    /// it back.
    async fn wheel_back(&self, mut c: Charge) -> Result<ChargeView, DeskError> {
        let now = crew::now();
        c.wheel = Wheel::Boss;
        c.wheel_since = Some(now);
        c.updated_at = now;
        let id = c.session;
        let shown = self.put_charge(c, Wheel::Boss).await?;
        self.charge_release_held(id).await;
        Ok(shown)
    }

    /// What `id`'s boss said to it while the person drove goes in: typed in
    /// now, and Desk's plugin, waiting for a message, hears of it.
    async fn charge_release_held(&self, id: Uuid) {
        match self.0.store.line_inbox(&id.to_string()).await {
            Ok(rows) => {
                for m in rows
                    .into_iter()
                    .filter(|m| m.from.kind == PartyKind::Manager && m.kind == LineKind::Tell)
                {
                    let _ = self.0.events.send(DeskEvent::Line {
                        message: Box::new(m),
                        removed: false,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "couldn't read what waited for a session")
            }
        }
        let desk = self.clone();
        tokio::spawn(async move { desk.line_pump(id).await });
    }

    /// Writes down each change of hands since last looked: the person took
    /// a wheel by their input, or it came back once their hold was up.
    async fn charge_settle(&self) {
        type Changed = (Charge, Wheel, Option<DateTime<Utc>>, chrono::Duration);
        let changed: Vec<Changed> = {
            let st = self.0.state.lock();
            st.charges
                .rows
                .values()
                .filter(|c| c.live())
                .filter_map(|c| {
                    let now = Self::wheel_now(&st, c);
                    let was = c.wheel;
                    (now != was && was != Wheel::PersonPinned).then(|| {
                        (
                            c.clone(),
                            now,
                            Self::last_input(&st, c.session),
                            Self::hold_of(&st, &c.boss),
                        )
                    })
                })
                .collect()
        };
        for (mut c, now, input, hold) in changed {
            let Ok(view) = self.view(c.session) else {
                continue;
            };
            let result = match now {
                Wheel::Person => {
                    c.wheel = Wheel::Person;
                    c.wheel_since = input.or(Some(crew::now()));
                    c.updated_at = crew::now();
                    let done = self.put_charge(c, Wheel::Person).await;
                    if done.is_ok() {
                        self.note(By::you().event(
                            Kind::Driving,
                            Some(Party::session(&view)),
                            format!("is driving {}", view.label()),
                        ));
                    }
                    done
                }
                Wheel::Boss => {
                    let boss = Party::manager(&c.boss, &self.boss_name(&c.boss));
                    let done = self.wheel_back(c).await;
                    if done.is_ok() {
                        self.note(crew::Event::new(
                            Kind::HandedBack,
                            boss,
                            Some(Party::session(&view)),
                            format!("has {} back after {}", view.label(), quiet_for(hold)),
                        ));
                    }
                    done
                }
                Wheel::PersonPinned => continue,
            };
            if let Err(e) = result {
                tracing::warn!(session = %view.id, error = %e, "couldn't write down who drives a session");
            }
        }
    }

    /// The charges' own loop: who has each wheel is written down as it
    /// changes hands, and a forgotten session's charge goes with it.
    pub(super) async fn charge_loop(self) {
        // Read again until they are, waiting longer each time.
        let mut wait = Duration::from_secs(1);
        while !self.charges_loaded() {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            if self.charge_load().await {
                break;
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
        let mut rx = self.0.events.subscribe();
        let mut tick = tokio::time::interval(SETTLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => self.charge_settle().await,
                ev = rx.recv() => match ev {
                    Ok(DeskEvent::Removed { id }) => {
                        let mut st = self.0.state.lock();
                        st.charges.rows.remove(&id);
                        st.charges.driven.remove(&id);
                    }
                    Ok(DeskEvent::Session { session })
                        if self.charge_of(session.id).is_some_and(|c| c.live()) =>
                    {
                        self.charge_settle().await
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }
}

/// How long the person was quiet before a wheel came back, in words:
/// "2 quiet minutes", "1 quiet minute", "30 quiet seconds".
fn quiet_for(hold: chrono::Duration) -> String {
    match hold.num_minutes() {
        0 => format!("{} quiet seconds", hold.num_seconds().max(1)),
        1 => "1 quiet minute".into(),
        n => format!("{n} quiet minutes"),
    }
}

/// The repository a session's own folder is: its `origin`'s address and
/// its checkouts (the folder itself and, for a worktree, the main one).
struct OwnRepo {
    origin: Option<String>,
    checkouts: Vec<std::path::PathBuf>,
    /// Every remote's every address, fetch and push, as git uses them.
    remotes: Vec<String>,
    /// It says `push.followTags`.
    follow_tags: bool,
    /// What `gh repo set-default` wrote: each `remote.*.gh-resolved`.
    gh_resolved: Vec<String>,
    /// It has git hooks: a file in its hooks folder (`core.hooksPath`'s,
    /// when set) that isn't one of git's samples.
    hooks: bool,
}

impl OwnRepo {
    fn named(&self, named: &[String], home: &std::path::Path) -> bool {
        charge::repo_named(
            self.origin.as_deref(),
            &self.checkouts,
            named,
            home,
            &crate::computer::boss::resolve_real,
        )
    }
}

/// The repository whose top is `own`, a session's own folder; `None` when
/// it is no repository's top (a nested folder, no repository at all).
fn repo_of_own(own: &std::path::Path) -> Option<OwnRepo> {
    use crate::computer::boss::resolve_real;
    let own_real = resolve_real(own)?;
    let top = git_line(own, &["rev-parse", "--show-toplevel"])
        .and_then(|t| resolve_real(std::path::Path::new(&t)))?;
    if top != own_real {
        return None;
    }
    let mut checkouts = vec![top];
    // A worktree's main checkout holds the repository it shares.
    if let Some(common) = git_line(
        own,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        let common = std::path::PathBuf::from(common);
        if let Some(main) = common
            .file_name()
            .filter(|n| *n == ".git")
            .and_then(|_| common.parent())
            .and_then(resolve_real)
        {
            if !checkouts.contains(&main) {
                checkouts.push(main);
            }
        }
    }
    let mut remotes: Vec<String> = git_lines(own, &["remote", "-v"])
        .iter()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect();
    remotes.extend(
        git_lines(
            own,
            &["config", "--get-regexp", r"^remote\..*\.(url|pushurl)$"],
        )
        .iter()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string)),
    );
    remotes.sort();
    remotes.dedup();
    Some(OwnRepo {
        // Where a push to it goes: its push address, when it has one.
        origin: git_line(own, &["remote", "get-url", "--push", "origin"]),
        checkouts,
        remotes,
        follow_tags: git_line(own, &["config", "--bool", "--get", "push.followTags"])
            .is_some_and(|v| v == "true"),
        gh_resolved: git_lines(
            own,
            &["config", "--get-regexp", r"^remote\..*\.gh-resolved$"],
        )
        .iter()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect(),
        hooks: has_hooks(own),
    })
}

/// Whether the repository at `own` has git hooks: anything in the folder
/// git runs them from that isn't one of its samples. One it can't read
/// counts as having them.
fn has_hooks(own: &std::path::Path) -> bool {
    let Some(dir) = git_line(
        own,
        &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
    ) else {
        return true;
    };
    match std::fs::read_dir(&dir) {
        Ok(entries) => entries.flatten().any(|e| {
            !e.file_name().to_string_lossy().ends_with(".sample")
                && e.file_type().is_ok_and(|t| !t.is_dir())
        }),
        // No hooks folder: none.
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// How long `origin` has to say where its default branch is.
const LS_REMOTE_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// [`git_line`], given `within` to answer, never asking for a password or
/// a passphrase; `None` when it doesn't answer in time.
fn git_line_within(
    dir: &std::path::Path,
    args: &[&str],
    within: std::time::Duration,
) -> Option<String> {
    use std::io::Read;
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=5",
        );
    }
    let mut child = cmd.spawn().ok()?;
    let until = std::time::Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if std::time::Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let line = out.lines().next().unwrap_or_default().trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Every line git prints in `dir`; none when it can't say.
fn git_lines(dir: &std::path::Path, args: &[&str]) -> Vec<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One line git prints in `dir`, or `None` when it can't say.
fn git_line(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Whether `command`, run in `cwd`, may change one of `settings`
/// (agents' own config files and folders, each inside its workspace), as
/// far as its words can tell. It errs towards yes, as Act fully allows
/// whatever it says no to. A word of it is held when, as a path (`~` and
/// `$HOME` read as home; a relative one read from `cwd`, from home, and
/// from every absolute path the command names, as a `cd` may have gone
/// there):
/// - it is inside a workspace, or names one of a workspace's own
///   folders above it short of home (`~/.workforce`, its `agents`);
/// - it can't be read (a variable, a substitution, a glob) and ends in,
///   or passes through, a settings file or folder's name.
///
/// Words can't catch everything a program might build at run time: the
/// budget is the person's because Desk's card is how it changes.
pub(crate) fn command_may_touch_settings(
    command: &str,
    cwd: &std::path::Path,
    settings: &[std::path::PathBuf],
    home: &std::path::Path,
) -> bool {
    use std::path::{Component, Path, PathBuf};
    // Only what may write is held: a command made only of reads, with no
    // output sent to a file, changes nothing.
    if !may_write(command) {
        return false;
    }
    // `p` as its links and `.`/`..` resolve, as far as it exists.
    fn real(p: &Path) -> PathBuf {
        let mut lexical = PathBuf::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    lexical.pop();
                }
                c => lexical.push(c),
            }
        }
        let mut head = lexical.clone();
        let mut tail = Vec::new();
        loop {
            if let Some(r) = crate::computer::boss::resolve_real(&head) {
                return tail
                    .iter()
                    .rev()
                    .fold(r, |acc, t: &std::ffi::OsString| acc.join(t));
            }
            match (head.file_name().map(|n| n.to_os_string()), head.parent()) {
                (Some(name), Some(up)) => {
                    tail.push(name);
                    head = up.to_path_buf();
                }
                _ => return lexical,
            }
        }
    }
    let home_real = real(home);
    let roots: Vec<PathBuf> = {
        let mut r: Vec<PathBuf> = settings
            .iter()
            .filter_map(|s| s.parent().map(real))
            .collect();
        r.sort();
        r.dedup();
        r
    };
    // A workspace, or a folder of its above it and short of home.
    let guarded: Vec<PathBuf> = roots
        .iter()
        .flat_map(|r| {
            r.ancestors()
                .take_while(|a| a.starts_with(&home_real) && *a != home_real)
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        })
        .chain(roots.iter().cloned())
        .collect();
    let names: Vec<String> = settings
        .iter()
        .filter_map(|s| s.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let home_s = home.display().to_string();
    // Quotes and escapes only join what they touch: `.work"force"` is
    // `.workforce` to the shell.
    let text = command
        .replace(['\'', '"', '\\'], "")
        .replace("${HOME}", &home_s)
        .replace("$HOME", &home_s);
    let words: Vec<String> = text
        .split(|c: char| c.is_whitespace() || ";|&()<>=:,".contains(c))
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "~" => home_s.clone(),
            w => match w.strip_prefix("~/") {
                Some(rest) => format!("{home_s}/{rest}"),
                None => w.to_string(),
            },
        })
        .collect();
    let mut bases = vec![real(cwd), home_real.clone()];
    bases.extend(
        words
            .iter()
            .filter(|w| w.starts_with('/'))
            .map(|w| real(Path::new(w))),
    );
    if bases.iter().any(|b| roots.iter().any(|r| b.starts_with(r))) {
        return true;
    }
    let held = |p: &Path| {
        let p = real(p);
        roots.iter().any(|r| p.starts_with(r)) || guarded.contains(&p)
    };
    // A word that can't be read (a variable, a substitution, a glob) may
    // be a path into a workspace, wherever the settings name is said.
    let unread = |w: &str| w.contains(['$', '`', '*', '?', '[']);
    // It ends in a settings file or folder's name (`config.toml`,
    // `$W/heartbeats`), not merely passes one (`~/.local/bin`).
    let through = |w: &str| {
        let last = w.trim_end_matches('/').rsplit('/').next().unwrap_or(w);
        names.iter().any(|n| n == last)
    };
    if words.iter().any(|w| unread(w)) && words.iter().any(|w| through(w)) {
        return true;
    }
    words.iter().any(|w| {
        if unread(w) {
            return false;
        }
        match w.starts_with('/') {
            true => held(Path::new(w)),
            false => bases.iter().any(|b| held(&b.join(w))),
        }
    })
}

/// Whether `command` may write anything: anything but a chain of plain
/// reads (`cat`, `grep`, `ls`, `sed` without `-i`, `find` without
/// `-exec` or `-delete`, …) with no output redirected to a file. What it
/// doesn't know counts as a write.
pub(crate) fn may_write(command: &str) -> bool {
    // Only programs that can't write a file, whatever they are given.
    const READS: &[&str] = &[
        "cat",
        "head",
        "tail",
        "less",
        "more",
        "grep",
        "egrep",
        "fgrep",
        "rg",
        "ls",
        "wc",
        "stat",
        "file",
        "du",
        "df",
        "pwd",
        "echo",
        "printf",
        "true",
        "test",
        "[",
        "diff",
        "cmp",
        "jq",
        "cut",
        "tr",
        "column",
        "nl",
        "basename",
        "dirname",
        "realpath",
        "readlink",
        "which",
        "type",
        "date",
        "id",
        "whoami",
        "uname",
        "hostname",
        "sed",
        "find",
        "od",
        "md5sum",
        "sha256sum",
        "cd",
    ];
    // What runs the program after it: judged by that program.
    const WRAPS: &[&str] = &["env", "command", "nice", "time", "nohup"];
    // Output into a file: `>` and `>>`, but not `2>&1` or `>/dev/null`.
    let cleaned = command
        .replace("2>&1", "")
        .replace(">&2", "")
        .replace(">/dev/null", "")
        .replace("> /dev/null", "")
        .replace("2>/dev/null", "");
    if cleaned.contains('>') || cleaned.contains("tee ") {
        return true;
    }
    cleaned
        .split(['|', ';', '&', '\n', '(', ')', '`'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .any(|segment| {
            let all: Vec<&str> = segment.split_whitespace().collect();
            let mut words = all.as_slice();
            // A variable set first (`M=/x; …`, `LC_ALL=C sort`) and a
            // wrapper with its flags write nothing themselves.
            loop {
                match words.first() {
                    Some(w) if w.contains('=') && !w.starts_with('-') => words = &words[1..],
                    Some(w) if WRAPS.contains(&w.rsplit('/').next().unwrap_or(w)) => {
                        words = &words[1..];
                        while words.first().is_some_and(|w| w.starts_with('-')) {
                            words = &words[1..];
                        }
                    }
                    _ => break,
                }
            }
            let Some(program) = words.first() else {
                return false;
            };
            let program = program.rsplit('/').next().unwrap_or(program);
            if !READS.contains(&program) {
                return true;
            }
            let bare = |w: &&str| w.trim_matches(['\'', '"']).to_string();
            match program {
                // `-i` in any spelling (`-Ei`, `-ni`, `--in-place=.bak`), or a
                // script that writes or runs a command: `w`, `W` or `e`
                // after an address or alone (`1,$w f`, `/re/!w f`, `e cmd`),
                // or as an `s` flag (`s/a/b/w f`, `s/a/b/e`).
                "sed" => words.iter().skip(1).any(|w| {
                    let w = bare(w);
                    if w.starts_with("--in-place") {
                        return true;
                    }
                    // A script given as `--expression=…` is read as one.
                    let w = match w.strip_prefix("--expression=") {
                        Some(script) => script.to_string(),
                        None if w.starts_with("--") => return false,
                        None if w.starts_with('-') => return w.contains('i'),
                        None => w,
                    };
                    let chars: Vec<char> = w.chars().collect();
                    chars.iter().enumerate().any(|(i, c)| {
                        let next = chars.get(i + 1);
                        "wWe".contains(*c)
                            && (i == 0 || "0123456789$/~,!;{} ".contains(chars[i - 1]))
                            && (next.is_none_or(|n| n.is_whitespace())
                                // `w` straight before its file: `1,$w/path`.
                                || (*c != 'e' && next.is_some_and(|n| "/~$.".contains(*n))))
                    })
                }),
                "find" => words.iter().any(|w| {
                    matches!(
                        *w,
                        "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fls"
                    ) || w.starts_with("-fprint")
                }),
                "rg" => words.iter().any(|w| w.starts_with("--pre")),
                _ => false,
            }
        })
}

#[cfg(test)]
#[path = "charge_tests.rs"]
mod tests;
