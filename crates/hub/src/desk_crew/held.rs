//! What a boss holds of the person's word, and its way to the session that
//! waits on it (VEL-22, 19 Nov: the person answered the plan's eight
//! questions in the DM at 11:57; the boss kept their answers as an order but
//! never gave them to the session, which stopped at 14:50 on "no recorded
//! answers", and the boss's update then told the person nothing was
//! answered).
//!
//! - **Held.** The boss's open orders (the person's words, kept from their
//!   DM or with `crew.order`) that cover a session: linked to it, or naming
//!   its ticket (or its name, when that is a ticket's key) in their words,
//!   as an order made before the session was is.
//! - **Given once.** [`CrewHub::deliver_held`] sends the session those words
//!   as the boss's own message, through every gate of one; it is not sent
//!   again while its line still has it, nor when the boss's own word has
//!   reached the session since it said it waits.
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

use super::actions::{Action, Target};
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

/// The boss's open orders in `open` that cover session `s` doing `work`:
/// linked to it, or naming one of its tickets (or its name, when that is a
/// ticket's key) in their words.
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
        .filter(|i| i.linked().contains(&s) || keys.iter().any(|k| names(&i.text, k)))
        .collect()
}

/// What the session is sent of `item`: the person's word, said as the
/// boss holds it.
fn message(item: &OpenItem) -> String {
    format!(
        "The person already gave their word on this, and I hold it: {}\n\nGo ahead on it; there is nothing to wait for.",
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

/// Whether the line `thread` already has what `item` holds, from `boss`
/// to `s`: its message, or any word of the boss's that reached the session
/// after `since` (when it said it waits).
fn given(
    thread: &[LineMessage],
    boss: &str,
    s: Uuid,
    item: &OpenItem,
    since: DateTime<Utc>,
) -> bool {
    // Its first words, its spacing aside.
    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let held: String = flat(&item.text).chars().take(80).collect();
    thread.iter().filter(|m| bosss(m, boss, s)).any(|m| {
        (m.at >= item.opened_at && flat(&m.text).contains(&held))
            || (m.delivery == Delivery::Delivered && m.delivered_at.unwrap_or(m.at) > since)
    })
}

impl CrewHub {
    /// Gives session `s` the person's word `boss` holds for it (`item`, an
    /// order of its), as the boss's message, once. True when it went now;
    /// false when the line has it already, or the boss's own word reached
    /// the session after `since` (when it said it waits). The error is
    /// why it couldn't go (a gate, its level).
    pub(crate) async fn deliver_held(
        &self,
        boss: &str,
        s: Uuid,
        item: &OpenItem,
        since: DateTime<Utc>,
    ) -> Result<bool, String> {
        let thread = self
            .desk
            .line_thread(s, None, Some(LOOK_BACK))
            .await
            .map_err(|e| format!("Desk couldn't read its line: {e}"))?;
        if given(&thread, boss, s, item, since) {
            return Ok(false);
        }
        let Some((service, _)) = self.overseer_named(boss) else {
            return Err("its boss isn't running here".into());
        };
        let by = self.by_boss(
            &service,
            "It waits on the person's word, which I hold: given to it.",
        );
        self.perform(
            &Action::Nudge {
                to: Target::Session(s),
                message: message(item),
                reply_to: None,
            },
            by,
        )
        .await?;
        tracing::info!(session = %s, item = %item.id, "the person's word a boss held went to the session waiting on it");
        Ok(true)
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
        let other = order("VEL-221: leave the export alone", vec![]);
        let linked = order("Keep the old index until Friday", vec![s]);
        let open = vec![answers.clone(), other, linked.clone()];
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
