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
        .filter(|i| write::asks_for_word(&i.text))
        .filter(|i| i.linked().contains(&s) || keys.iter().any(|k| names(&i.text, k)))
        .collect()
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

/// When the line `thread` already had what `item` holds, from `boss` to
/// `s`: its message (sent, if not yet delivered), or any word of the
/// boss's that reached the session after `since` (when it said it waits).
fn given(
    thread: &[LineMessage],
    boss: &str,
    s: Uuid,
    item: &OpenItem,
    since: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    // Its first words, its spacing aside.
    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let held: String = flat(&item.text).chars().take(80).collect();
    let reached = |m: &LineMessage| m.delivered_at.unwrap_or(m.at);
    thread
        .iter()
        .filter(|m| bosss(m, boss, s))
        .filter(|m| {
            (m.at >= item.opened_at && flat(&m.text).contains(&held))
                || (m.delivery == Delivery::Delivered && reached(m) > since)
        })
        .map(reached)
        .max()
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
        if let Some(at) = given(&thread, boss, s, item, since) {
            return Held::Given(at);
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
        Held::Sent
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
        // An order that answers nothing is no held answer.
        let kept = order("Keep the old index until Friday", vec![s]);
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
