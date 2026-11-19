//! The bosses' one live picture on the hub's side
//! ([`workforce_desk::picture`]; designs/desk-boss-one-picture.md): what
//! the person says to a boss in its DM (or another boss relays as theirs),
//! each instruction any boss's turn sends a session, and the person's
//! standing orders, written to the picture as they happen. Desk writes the
//! rest: the person's words into a session, and what a session says on
//! its line.
//!
//! **One lane per session, checked at send.** Every boss turn (its DM, a
//! session's line, a work or plan turn, a check-in) and every focused Act
//! fully call notes the picture's number as it starts: what it decides
//! on. Whatever it then tells a session goes through that session's lane
//! in [`CrewHub::perform`], one at a time: if anything touching the
//! session was written to the picture since it started (the person's
//! words, another turn's instruction, what the session said, a shared
//! fact, a standing order), it is not sent; the turn is told what changed
//! and decides again on it. Sessions' lanes are their own, so a change to
//! one never holds another back.
//!
//! **A repeat is dropped.** An instruction from a later turn or call
//! while the session's last one still stands isn't sent: it is carrying
//! that one out. It stands while it was sent in the last
//! [`REPEAT_WINDOW`] and since then nothing touching the session was
//! written to the picture, its account says nothing done, and it hasn't
//! stopped with nothing of its own running (a turn that ends while its
//! watcher runs is not news).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_desk::picture::{Happened, Note, What};

use super::actions::{Action, Target};
use super::CrewHub;

/// How long a boss's instruction to a session stands as its current one
/// for [`CrewHub::check_at_send`]'s repeats.
pub(super) const REPEAT_WINDOW: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// How a refusal for a picture that moved starts: a focused call that
/// meets it decides again.
pub(crate) const STALE: &str = "Not sent: since you decided this,";

/// What every boss turn reads once the day's metered cheap calls are
/// spent.
/// Opens the crew block while it holds (`super::context::block`): accounts
/// said plainly read like fresh ones, and are never to be taken for them.
pub(super) const NOTES_SPENT: &str = "Sessions' summaries are written plainly from their last words for the rest of today: the day's metered cheap calls are spent (`[oversight.updates] cheap_calls_per_day`). They may be out of date: check a session (crew.account, crew.ask_session) before you say anything has or hasn't happened.";

/// How long a turn's basis is kept, at most, when its end isn't seen.
const BASIS_KEPT: chrono::TimeDelta = chrono::TimeDelta::hours(6);

/// The rows a refusal says, at most.
const SAID_ROWS: usize = 6;

/// What one boss turn or focused call decides on: its own key (its rows
/// are its own), and the picture's number when it began.
#[derive(Debug, Clone)]
pub(super) struct Deciding {
    pub turn: String,
    pub basis: u64,
}

tokio::task_local! {
    /// A focused call's [`Deciding`], for the call's whole run.
    static DECIDING: Deciding;
}

/// Each running boss turn's [`Deciding`], by its runner session.
pub(super) type Bases = HashMap<String, (Deciding, DateTime<Utc>)>;

/// One lane per session: what is sent it goes one at a time.
pub(super) type Lanes = HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>;

/// Turns (and focused calls) told their picture moved for a session: the
/// newest row each was told for it, and when.
pub(super) type Redecided = HashMap<(String, Uuid), (u64, std::time::Instant)>;

/// How many moved rows a refusal lists: the basis moves only past them.
const TOLD_ROWS: usize = 20;

/// Whether `refused` is a refusal for a picture that moved: decide again.
pub(crate) fn stale(refused: &str) -> bool {
    refused.starts_with(STALE)
}

impl CrewHub {
    /// The person's own words to `boss` in its DM, as they reach it:
    /// before its turn on them starts, so every turn of the boss's already
    /// running on what it knew before is decided again.
    pub(super) fn picture_person_told(&self, boss: &str, words: &str) {
        if words.trim().is_empty() {
            return;
        }
        self.desk
            .picture_note(Note::new(What::PersonSaid, words).boss(boss));
    }

    /// `boss` told `session` `text` from its turn `turn` (none: not from a
    /// turn of its own, as when the person accepts a suggestion).
    pub(super) fn picture_instruction(
        &self,
        boss: &str,
        session: Uuid,
        text: &str,
        turn: Option<String>,
    ) {
        self.desk.picture_note(
            Note::new(What::Instruction, text)
                .boss(boss)
                .session(session)
                .turn(turn),
        );
    }

    /// One of the person's standing orders was kept, changed, paused or
    /// forgotten (`what`, in plain words): every boss's every session.
    pub(crate) fn picture_order(&self, what: &str, turn: Option<String>) {
        self.desk
            .picture_note(Note::new(What::Order, what).turn(turn));
    }

    /// The turn or focused call this runs in, by its key.
    pub(super) fn current_turn(&self) -> Option<String> {
        self.deciding_now().map(|d| d.turn)
    }

    /// What the turn or focused call this runs in decides on: none
    /// outside one (the person, accepting a suggestion).
    pub(super) fn deciding_now(&self) -> Option<Deciding> {
        if let Ok(d) = DECIDING.try_with(Clone::clone) {
            return Some(d);
        }
        let sid = workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(Clone::clone)
            .ok()?;
        self.turn_bases.lock().get(&sid).map(|(d, _)| d.clone())
    }

    /// A boss's turn on runner session `sid` begins now (or takes in the
    /// person's next message): it decides on the picture as it is.
    /// Its key: the turn's own, for [`Self::picture_turn_ended`].
    pub(super) fn picture_turn_began(&self, sid: &str) -> String {
        let basis = self.desk.picture_seq();
        let now = Utc::now();
        let turn = format!("{sid}#{basis}#{}", Uuid::new_v4().simple());
        let mut bases = self.turn_bases.lock();
        bases.retain(|_, (_, at)| now - *at < BASIS_KEPT);
        bases.insert(
            sid.to_string(),
            (
                Deciding {
                    turn: turn.clone(),
                    basis,
                },
                now,
            ),
        );
        turn
    }

    /// The turn on `old` goes on as `new` (its runner session rolled over).
    pub(super) fn picture_turn_rolled(&self, old: &str, new: &str) {
        let mut bases = self.turn_bases.lock();
        if let Some(b) = bases.get(old).cloned() {
            bases.insert(new.to_string(), b);
        }
    }

    /// The boss's turn `turn` on `sid` ended: its basis goes, unless a
    /// later turn on `sid` already began.
    pub(super) fn picture_turn_ended(&self, sid: &str, turn: &str) {
        let mut bases = self.turn_bases.lock();
        if bases.get(sid).is_some_and(|(d, _)| d.turn == turn) {
            bases.remove(sid);
        }
    }

    /// Runs the focused call `f` on the picture as it is now.
    pub(super) async fn on_picture<F: std::future::Future>(&self, f: F) -> F::Output {
        let d = Deciding {
            turn: format!("call:{}", Uuid::new_v4()),
            basis: self.desk.picture_seq(),
        };
        DECIDING.scope(d, f).await
    }

    /// What the boss's turn on runner session `sid` decided on, while kept.
    pub(super) fn turn_deciding(&self, sid: &str) -> Option<Deciding> {
        self.turn_bases.lock().get(sid).map(|(d, _)| d.clone())
    }

    /// `action` performed as decided in `d` (a turn that has ended, whose
    /// reply is acted on after it).
    pub(super) async fn perform_in(
        &self,
        d: Option<Deciding>,
        action: &Action,
        by: workforce_desk::crew::By,
    ) -> Result<String, String> {
        match d {
            Some(d) => DECIDING.scope(d, self.perform(action, by)).await,
            None => self.perform(action, by).await,
        }
    }

    /// Session `s`'s lane, held: whatever goes to it waits its turn.
    pub(super) async fn lane(&self, s: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lane = {
            let mut lanes = self.lanes.lock();
            // Lanes no one holds or waits on go.
            if lanes.len() > 256 {
                lanes.retain(|_, l| Arc::strong_count(l) > 1);
            }
            Arc::clone(lanes.entry(s).or_default())
        };
        lane.lock_owned().await
    }

    /// The session a boss's `action` tells something, in its lane.
    pub(super) fn lane_of(action: &Action, by: &workforce_desk::crew::By) -> Option<Uuid> {
        if by.party.kind != workforce_desk::crew::PartyKind::Manager {
            return None;
        }
        // Whatever carries the boss's words (or work) to the session.
        match action {
            Action::Nudge {
                to: Target::Session(s),
                ..
            }
            | Action::AssignTicket {
                to: super::actions::Handout::Session(s),
                ..
            }
            | Action::Resume {
                session: s,
                prompt: Some(_),
            }
            | Action::Restart {
                session: s,
                prompt: Some(_),
            } => Some(*s),
            _ => None,
        }
    }

    /// `action`, a boss's message to session `s`, checked against the
    /// picture as it is now, in `s`'s lane: `Err` (starting [`STALE`]) when
    /// the picture moved since the turn decided it, `Ok(Some)` when it
    /// repeats the session's current instruction, `Ok(None)` to send it.
    pub(super) async fn check_at_send(
        &self,
        action: &Action,
        boss: &str,
        s: Uuid,
    ) -> Result<Option<String>, String> {
        let tz = self.desk.timezone();
        let name = self.session_name(s);
        // The person drives it: they have the wheel of a session a boss
        // runs. No instruction of the boss's goes to it then, and none
        // waits to go in over what they do there. Handed back, the boss is
        // woken, and its picture has what they said in it.
        // - Driving is measured in the hub's own minutes too (the gap its
        //   focused calls wait out, never shorter than the hold), so a call
        //   that waited out the person never ends refused. With real
        //   minutes that is exactly the wheel.
        // - An answer on its line to a question the session asked
        //   (`reply_to`) still goes, as Desk lets it: the session waits on
        //   it.
        // - A session the boss only oversees has no wheel: the focused
        //   calls wait out the person's input there themselves.
        let instruction = matches!(action, Action::Nudge { reply_to: None, .. });
        let gap = || {
            self.overseer_named(boss).map_or_else(
                || self.answer_gap(&workforce_common::oversight::Oversight::default()),
                |(_, o)| self.answer_gap(&o),
            )
        };
        if instruction
            && self.desk.wheel_of(s).is_some_and(|w| w.person())
            && self.desk.person_in_control(s, gap())
        {
            return Ok(Some(format!(
                "not sent: the person is driving {name} now, so nothing of yours goes to it. When they hand it back (or stop giving it input for a while) you're woken, with what they said in it, and decide then. If the person asked you to tell it this, say so to them in your DM instead."
            )));
        }
        let Some(d) = self.deciding_now() else {
            return Ok(None);
        };
        // Told since it began what moved: it decides on that now.
        let basis = self
            .redecided
            .lock()
            .get(&(d.turn.clone(), s))
            .map_or(d.basis, |(seq, _)| d.basis.max(*seq));
        let moved = self.desk.picture_since(boss, s, basis, Some(&d.turn));
        if !moved.is_empty() {
            // Told now: what it sends next stands on this.
            self.decided_again(&d, s, &moved);
            tracing::info!(session = %s, %boss, turn = %d.turn, rows = moved.len(), "a boss's message decided on an old picture wasn't sent; it decides again");
            return Err(format!(
                "{STALE} the picture moved for {name}:\n{}\nDecide again on this: send it a new message only if one is still needed and fits what is above, or nothing.",
                moved[..moved.len().min(TOLD_ROWS)]
                    .iter()
                    .map(|r| format!("- {}", r.said(tz)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let Action::Nudge { reply_to: None, .. } = action else {
            return Ok(None);
        };
        let Some(current) = self.desk.picture_last_instruction(boss, s) else {
            return Ok(None);
        };
        let own = current
            .turn
            .as_deref()
            .is_some_and(|t| t == d.turn.as_str());
        // Judged by what changed, never by wording: with nothing new since
        // the instruction it carries out, another turn's is a repeat.
        if own || Utc::now() - current.at >= REPEAT_WINDOW {
            return Ok(None);
        }
        let since = self.desk.picture_since(boss, s, current.seq, None);
        if !since.is_empty() || self.session_did_since(s, current.at).await {
            return Ok(None);
        }
        tracing::info!(session = %s, %boss, "a boss's message repeating what the session is carrying out wasn't sent");
        Ok(Some(format!(
            "not sent: {name} already has your instruction from {} (“{}”), and nothing about it has changed since: it is carrying it out. Tell it something only once something changes.",
            current.at.with_timezone(&tz).format("%H:%M"),
            workforce_desk::crew::one_line(&current.text, 160)
        )))
    }

    /// The turn `d` was told what changed: it decides on the picture now.
    /// It was told `moved` for session `s` only: for `s`, it decides from
    /// the newest of them (what it wasn't told still makes it stale), and
    /// for every other session on what it began with.
    fn decided_again(&self, d: &Deciding, s: Uuid, moved: &[Happened]) {
        let shown = &moved[..moved.len().min(TOLD_ROWS)];
        let Some(newest) = shown.iter().map(|r| r.seq).max() else {
            return;
        };
        let mut told = self.redecided.lock();
        told.retain(|_, (_, at)| at.elapsed() < std::time::Duration::from_secs(6 * 3600));
        told.insert((d.turn.clone(), s), (newest, std::time::Instant::now()));
    }

    /// Whether session `s` moved on since `at`: its account says it did
    /// something, or it stopped since with nothing of its own still
    /// running (a turn that ends while its watcher runs is it carrying the
    /// instruction out). Unreadable, it is taken that it did, so nothing is
    /// held back.
    async fn session_did_since(&self, s: Uuid, at: DateTime<Utc>) -> bool {
        if let Ok(v) = self.desk.view(s) {
            let stopped = matches!(
                v.state,
                workforce_desk::model::SessionState::Done
                    | workforce_desk::model::SessionState::Idle
            );
            if stopped && v.background.is_none() && v.updated_at > at {
                return true;
            }
        }
        match self.desk.session_accounts_of(&[s]).await {
            Ok(a) => a
                .first()
                .is_some_and(|a| !workforce_desk::update::done_since(None, a, at).is_empty()),
            Err(_) => true,
        }
    }

    /// What a boss turn about `session` (or its DM, with none) reads of
    /// the picture: what touched the session lately, the person's words to
    /// it, facts and orders, and its last instruction to each other
    /// session it runs.
    pub(super) fn picture_block(
        &self,
        boss: &str,
        about: Option<Uuid>,
        others: &[(Uuid, String)],
    ) -> Option<String> {
        let tz = self.desk.timezone();
        let mut out = String::new();
        let mine = self.desk.picture_recent_for(boss, SAID_ROWS);
        if let Some(r) = workforce_desk::picture::render(&mine, tz) {
            out.push_str("What the person told you lately, and the facts and standing orders that hold, oldest first:\n");
            out.push_str(&r);
            out.push('\n');
        }
        if let Some(s) = about {
            let rows: Vec<Happened> = self
                .desk
                .picture_recent(boss, s, SAID_ROWS * 2)
                .into_iter()
                .filter(|r| r.session.is_some())
                .collect();
            if let Some(r) = workforce_desk::picture::render(&rows, tz) {
                out.push_str("What happened for the session this turn is about, oldest first (the person's words in it, your instructions from every turn, what it said):\n");
                out.push_str(&r);
                out.push('\n');
            }
        }
        let last: Vec<String> = others
            .iter()
            .filter(|(id, _)| Some(*id) != about)
            .filter_map(|(id, name)| {
                let i = self.desk.picture_last_instruction(boss, *id)?;
                (Utc::now() - i.at < chrono::TimeDelta::hours(2)).then(|| {
                    format!(
                        "- {name}: {} “{}”",
                        i.at.with_timezone(&tz).format("%H:%M"),
                        workforce_desk::crew::one_line(&i.text, 160)
                    )
                })
            })
            .collect();
        if !last.is_empty() {
            out.push_str("Your last instruction to each session, from any turn of yours:\n");
            out.push_str(&last.join("\n"));
            out.push('\n');
        }
        (!out.is_empty()).then_some(out)
    }
}
