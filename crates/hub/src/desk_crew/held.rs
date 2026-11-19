//! What a boss holds of the person's word, and its way to the session that
//! waits on it (VEL-22, 19 Nov: the person answered the plan's eight
//! questions in the DM at 11:57; the boss kept their answers as an order but
//! never gave them to the session, which stopped at 14:50 on "no recorded
//! answers", and the boss's update then told the person nothing was
//! answered).
//!
//! - **Held.** The boss's open orders (the person's words, kept from their
//!   DM or with `crew.order`) that answer something (an answer, approval,
//!   decision) and cover a session: linked to it, or naming its ticket (or
//!   its name, when that is a ticket's key) in their words, as an order
//!   made before the session was is. "Keep the old index until Friday" is
//!   an order, not an answer: never passed off as one.
//! - **Given once.** [`CrewHub::deliver_held`] sends the session those words
//!   as the boss's own message, through the gate any message of the boss's
//!   goes through (its level, `act.nudge`, a session in another terminal);
//!   it is not sent again while its line still has it, and says when it
//!   went ([`Held::Given`]), so a session still waiting after is never told
//!   "passing them now" again.
//!
//! The update writer ([`super::stalls`]) and the wait looked at each minute
//! ([`super::waiting`]) both call it; so may anything else that finds a
//! session waiting on what the boss holds.

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_desk::crew::PartyKind;
use workforce_desk::line::{Delivery, LineMessage};
use workforce_desk::open_item::{OpenItem, OpenKind, OpenState};
use workforce_desk::update_write::{self as write, Work};

use super::actions::{Action, Gate, Target};
use super::CrewHub;

/// How far back a session's line is read for what the boss gave it.
const LOOK_BACK: i64 = 200;

/// The most characters of the held words a message carries.
const HELD_CHARS: usize = 3_000;

/// Whether `text` names `key` as a word: "VEL-22's" does, "VEL-221" doesn't.
fn names(text: &str, key: &str) -> bool {
    let (t, k) = (text.to_lowercase(), key.to_lowercase());
    t.match_indices(&k).any(|(at, _)| {
        let before = t[..at].chars().next_back();
        let after = t[at + k.len()..].chars().next();
        let joins = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
        !before.is_some_and(joins) && !after.is_some_and(|c| c.is_ascii_digit() || joins(c))
    })
}

/// What became of the person's word a boss holds, for a session waiting
/// on it ([`CrewHub::deliver_held`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Held {
    /// Sent to the session now.
    Sent,
    /// Sent before, still queued on its line: on its way.
    Going,
    /// Left on its line for a session not running (now or before), and
    /// the session started again through the gate to read it.
    Resumed,
    /// Left on its line for a session not running, which the boss may not
    /// start again (why): the person's to resume.
    Parked(String),
    /// It reached the session already, at this time (it, or the boss's own
    /// word after the session said it waits).
    Given(DateTime<Utc>),
    /// The boss may only suggest sending it: why.
    Suggested(String),
    /// It couldn't go: why.
    Refused(String),
}

/// The boss's open orders in `open` that answer something and cover
/// session `s` doing `work`: linked to it, or naming one of its tickets
/// (or its name, when that is a ticket's key) in their words.
pub(crate) fn held_for<'a>(open: &'a [OpenItem], s: Uuid, work: &Work) -> Vec<&'a OpenItem> {
    let mut keys: Vec<String> = work
        .titled
        .iter()
        .filter(|t| t.kind == workforce_desk::account::RefKind::Ticket)
        .map(|t| t.id.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    if write::is_id(&work.name) && !keys.iter().any(|k| k.eq_ignore_ascii_case(&work.name)) {
        keys.push(work.name.trim().to_string());
    }
    open.iter()
        .filter(|i| i.kind == OpenKind::Order && i.state != OpenState::Closed)
        .filter(|i| answers(&i.text))
        .filter(|i| i.linked().contains(&s) || keys.iter().any(|k| names(&i.text, k)))
        .collect()
}

/// Words an answer to something asked is given in: "yes", "go with option
/// B", "approved", beside what asks ([`write::asks_for_word`]).
const ANSWER_WORDS: &[&str] = &[
    "yes", "ok", "okay", "agreed", "go with", "option", "chose", "choose", "take the", "ship it",
    "go ahead",
];

/// Whether an order answers something asked ("ok yes to all to approve the
/// design", "go with option B"), not a standing instruction ("Keep the old
/// index until Friday") or more work ("also add tests for the importer").
fn answers(text: &str) -> bool {
    let l = text.to_lowercase();
    !standing(text) && (write::asks_for_word(text) || ANSWER_WORDS.iter().any(|w| names(&l, w)))
}

/// Whether an order is a standing instruction ("Keep the old index until
/// Friday", "Note: don't deploy"), not an answer to something asked: its
/// first words (past an id or a "Note:") say a rule.
fn standing(text: &str) -> bool {
    let l = text.replace('\u{2019}', "'");
    let rules = [
        "keep ",
        "don't ",
        "do not ",
        "never ",
        "always ",
        "stop ",
        "leave ",
        "wait until ",
        "hold off ",
        "hold the ",
        "hold it ",
    ];
    l.split([':', '\n', '.', ';'])
        .map(str::trim)
        // An id is read as written ("VEL-22"); the rest in lower case.
        .find(|p| {
            !p.is_empty()
                && !write::is_id(p)
                && !matches!(p.to_lowercase().as_str(), "note" | "fyi" | "ps")
        })
        .is_some_and(|p| {
            let p = p.to_lowercase();
            rules.iter().any(|w| p.starts_with(w))
        })
}

/// What the session is sent of `item`: the person's word, said as the
/// boss holds it.
fn message(item: &OpenItem) -> String {
    format!(
        "The person already gave their word on this, and I hold it: {}\n\nIf it answers what you wait on, go ahead on it; if it doesn't, say exactly what is still open.",
        workforce_desk::crew::clip(item.text.trim(), HELD_CHARS)
    )
}

/// Whether `m` is `boss`'s word to session `s` that went, or will.
fn bosss(m: &LineMessage, boss: &str, s: Uuid) -> bool {
    m.from.kind == PartyKind::Manager
        && m.from.id == boss
        && m.to_session() == Some(s)
        && !matches!(
            m.delivery,
            Delivery::Refused | Delivery::Withdrawn | Delivery::Noted
        )
}

/// Whether the line `thread` already has what `item` holds, from `boss` to
/// `s`: its message, on its way or delivered, or any word of the boss's
/// that reached the session after `since` (when it said it waits).
fn given(
    thread: &[LineMessage],
    boss: &str,
    s: Uuid,
    item: &OpenItem,
    since: DateTime<Utc>,
) -> Option<Given> {
    // Its first words, its spacing aside.
    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let held: String = flat(&item.text).chars().take(80).collect();
    let reached = |m: &LineMessage| m.delivered_at.unwrap_or(m.at);
    let ours = |m: &&LineMessage| m.at >= item.opened_at && flat(&m.text).contains(&held);
    let mine: Vec<&LineMessage> = thread.iter().filter(|m| bosss(m, boss, s)).collect();
    // Still on its way: going now, not given.
    if mine
        .iter()
        .any(|m| ours(m) && matches!(m.delivery, Delivery::Queued | Delivery::Handed))
    {
        return Some(Given::Going);
    }
    // Left for a session not running: it goes when it runs, never again.
    if mine.iter().any(|m| ours(m) && m.delivery == Delivery::Held) {
        return Some(Given::Parked);
    }
    mine.iter()
        .filter(|m| m.delivery == Delivery::Delivered && (ours(m) || reached(m) > since))
        .map(|m| reached(m))
        .max()
        .map(Given::At)
}

/// How what a boss holds already went to a session.
enum Given {
    /// Queued on its line for the running session, not yet read (one held
    /// for a session not running isn't on its way).
    Going,
    /// Left on its line for a session not running: it goes when it runs.
    Parked,
    /// It reached the session then.
    At(DateTime<Utc>),
}

impl CrewHub {
    /// Gives session `s` the person's word `boss` holds for it (`item`, an
    /// order of its), as the boss's message, once, through the gate any
    /// message of the boss's goes through ([`Held`]).
    pub(crate) async fn deliver_held(
        &self,
        boss: &str,
        s: Uuid,
        item: &OpenItem,
        since: DateTime<Utc>,
    ) -> Held {
        let thread = match self.desk.line_thread(s, None, Some(LOOK_BACK)).await {
            Ok(t) => t,
            Err(e) => return Held::Refused(format!("Desk couldn't read its line: {e}")),
        };
        match given(&thread, boss, s, item, since) {
            Some(Given::Going) => return Held::Going,
            Some(Given::Parked) => return self.resume_for_held(boss, s).await,
            Some(Given::At(at)) => return Held::Given(at),
            None => {}
        }
        let Some((service, o)) = self.overseer_named(boss) else {
            return Held::Refused("its boss isn't running here".into());
        };
        let action = Action::Nudge {
            to: Target::Session(s),
            message: message(item),
            reply_to: None,
        };
        match self.gate(&action, &o) {
            Gate::Run => {}
            Gate::Suggest(why) => return Held::Suggested(why.into_owned()),
            Gate::Refuse => return Held::Suggested("it only watches".into()),
            Gate::No(why) => return Held::Refused(why.into_owned()),
        }
        let by = self.by_at_level(
            self.by_boss(
                &service,
                "It waits on the person's word, which I hold: given to it.",
            ),
            &o,
        );
        if let Err(why) = self.perform(&action, by).await {
            return Held::Refused(why);
        }
        tracing::info!(session = %s, item = %item.id, "the person's word a boss held went to the session waiting on it");
        // A session not running has it when it runs again: started again.
        match self.desk.view(s) {
            Ok(v) if !v.live => self.resume_for_held(boss, s).await,
            _ => Held::Sent,
        }
    }

    /// Starts session `s` again, through the gate any resume of `boss`'s
    /// goes through, to read the person's word left on its line for it:
    /// [`Held::Resumed`], or [`Held::Parked`] with why it is the person's.
    async fn resume_for_held(&self, boss: &str, s: Uuid) -> Held {
        // Running (or started) already: it reads them in Desk's next pass.
        if self.desk.view(s).is_ok_and(|v| v.live) {
            return Held::Going;
        }
        let Some((service, o)) = self.overseer_named(boss) else {
            return Held::Parked("its boss isn't running here".into());
        };
        let action = Action::Resume {
            session: s,
            prompt: None,
        };
        match self.gate(&action, &o) {
            Gate::Run => {}
            Gate::Suggest(why) | Gate::No(why) => return Held::Parked(why.into_owned()),
            Gate::Refuse => return Held::Parked("it only watches".into()),
        }
        let by = self.by_at_level(
            self.by_boss(
                &service,
                "The person's word waits on its line and it isn't running: started again to read it.",
            ),
            &o,
        );
        match self.perform(&action, by).await {
            Ok(_) => {
                tracing::info!(session = %s, "a session the person's word waited for was started again");
                Held::Resumed
            }
            // Started meanwhile, or still starting: on its way.
            Err(_) if self.desk.view(s).is_ok_and(|v| v.live) => Held::Resumed,
            Err(why) if why.contains("may still be starting") => Held::Resumed,
            Err(why) => Held::Parked(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_desk::account::RefKind;
    use workforce_desk::update_write::Titled;

    fn order(text: &str, sessions: Vec<Uuid>) -> OpenItem {
        let now = Utc::now();
        OpenItem {
            id: Uuid::new_v4(),
            boss: "desk".into(),
            owner: "kingsley".into(),
            kind: OpenKind::Order,
            session: None,
            line_message: None,
            text: text.into(),
            holding_up: None,
            recommendation: None,
            options: vec![],
            generalizes: None,
            done_when: None,
            dedupe_key: format!("order:text::{}", Uuid::new_v4()),
            opened_at: now,
            last_said_at: None,
            said_count: 0,
            raised_at: None,
            sessions,
            prefer_scope: None,
            changed_at: now,
            state: OpenState::Open,
            closed_at: None,
            closed_by: None,
            closed_why: None,
            layout: Default::default(),
            cant_decide: None,
        }
    }

    /// The order from 19 Nov names its session's ticket, with no session
    /// linked: it is held for that session, and for no other.
    #[test]
    fn an_order_naming_a_sessions_ticket_is_held_for_it() {
        let s = Uuid::new_v4();
        let work = Work {
            session_id: s.to_string(),
            name: "VEL-22".into(),
            closed: false,
            goal: "Carry out the track 2 Workforce PRs".into(),
            titled: vec![Titled {
                kind: RefKind::Ticket,
                id: "VEL-22".into(),
                title: Some("Track 2 Workforce PRs".into()),
            }],
        };
        let answers = order(
            "VEL-22's eight section 7 design questions (cutover window 60-90 min, …): Kingsley, 19 Nov 11:57: \"ok yes to all to approve the design\".",
            vec![],
        );
        let other = order("VEL-221: the export questions are approved", vec![]);
        let linked = order("Approved: take the 60 minute cutover window", vec![s]);
        // A standing instruction is no held answer; a plain answer is.
        let kept = order("Keep the old index until Friday", vec![s]);
        let plain = order("VEL-22: go with option B, the 60 minute window", vec![]);
        assert_eq!(held_for(std::slice::from_ref(&plain), s, &work).len(), 1);
        let both = order(
            "VEL-22: go with option B. Keep the 60 minute window",
            vec![],
        );
        assert_eq!(held_for(std::slice::from_ref(&both), s, &work).len(), 1);
        for not in [
            "VEL-22: also add tests for the importer",
            "VEL-22. Note: don't deploy it",
            "VEL-22: pick up the importer tests too",
            "VEL-22: wait until Friday, then ship it",
            "VEL-22: hold the release until the approval lands",
        ] {
            assert!(
                held_for(&[order(not, vec![s])], s, &work).is_empty(),
                "{not}"
            );
        }
        let open = vec![answers.clone(), other, linked.clone(), kept];
        let held: Vec<Uuid> = held_for(&open, s, &work).iter().map(|i| i.id).collect();
        assert_eq!(held, vec![answers.id, linked.id]);
        // What isn't an order is none of the person's word.
        let mut promise = answers.clone();
        promise.kind = OpenKind::Promise;
        assert!(held_for(&[promise], s, &work).is_empty());
        assert!(names("see (VEL-22).", "VEL-22"));
        assert!(!names("VEL-220 is next", "VEL-22"));
        assert!(!names("XVEL-22", "VEL-22"));
    }
}
