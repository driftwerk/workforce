//! The person typing to a boss, and away from it (overseer H8).
//!
//! - **The typing hold.** While the person types in the boss's DM, Desk
//!   pings every few seconds. What the boss would post waits until a pause
//!   ([`QUIET`] after the last ping), and never longer than [`CAP`] from
//!   when the typing began: someone who leaves a half-typed message is not
//!   typing ([`Typing`]).
//! - **Away.** The person is away once the boss's posts in the DM have
//!   gone unread for `away_after_minutes` ([`away_since`]): judged from how
//!   far they have read the DM, so hours in session terminals with the DM
//!   unread count as away. Desk also says when the DM is on screen; while
//!   it does, they are here. When those pings stop, the read point alone
//!   decides.
//! - **The catch-up.** On their return, one message covers the absence:
//!   a headline ([`headline`]), what finished and what's open, each
//!   decision with its time and reason, how much the boss handled itself
//!   ([`handled_line`]), and what still waits on them, parked items by
//!   title ([`MentionedAway`], [`render`]).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::decision::Handled;
use crate::update::MentionedUpdate;

/// How long after the person's last keystroke ping a held post waits, in
/// minutes (20 s).
pub const QUIET: f64 = 1.0 / 3.0;

/// The most a post is held for the person's typing, in minutes.
pub const CAP: f64 = 3.0;

/// How long a presence ping from Desk counts, in minutes: Desk sends one
/// each minute and on every change, so a Desk gone quiet this long is
/// closed, older, or cut off.
pub const PRESENCE_FRESH: f64 = 3.0;

/// The person's typing in a boss's DM: when this stretch of it began, and
/// the last ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Typing {
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
}

impl Typing {
    /// A ping at `at`, after `was`: a ping after a pause of `quiet` or more
    /// begins a new stretch.
    pub fn ping(was: Option<Typing>, at: DateTime<Utc>, quiet: Duration) -> Typing {
        match was {
            Some(t) if at - t.last < quiet => Typing {
                first: t.first,
                last: at.max(t.last),
            },
            _ => Typing {
                first: at,
                last: at,
            },
        }
    }

    /// When what it holds may go: `quiet` after the last ping, and at the
    /// latest `cap` after the stretch began.
    pub fn release_at(&self, quiet: Duration, cap: Duration) -> DateTime<Utc> {
        (self.last + quiet).min(self.first + cap)
    }

    /// Whether it holds posts at `now`.
    pub fn holds(&self, now: DateTime<Utc>, quiet: Duration, cap: Duration) -> bool {
        now < self.release_at(quiet, cap)
    }
}

/// What Desk last said of the person: whether its window is attended, and
/// which boss's DM is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub at: DateTime<Utc>,
    pub attending: bool,
    pub dm_on_screen: Option<String>,
}

impl Presence {
    /// Whether it still counts at `now`.
    pub fn fresh(&self, now: DateTime<Utc>, fresh: Duration) -> bool {
        now - self.at <= fresh
    }

    /// Whether, at `now`, it says `boss`'s DM is in front of the person.
    pub fn shows(&self, boss: &str, now: DateTime<Utc>, fresh: Duration) -> bool {
        self.fresh(now, fresh) && self.attending && self.dm_on_screen.as_deref() == Some(boss)
    }
}

/// Since when the person is away from a boss's DM at `now`, or none: the
/// first of the boss's `posts` after `seen` (how far they read it, or last
/// wrote in it), once it has gone unread `after`. Never while the DM is
/// `on_screen`.
pub fn away_since(
    posts: &[DateTime<Utc>],
    seen: DateTime<Utc>,
    on_screen: bool,
    now: DateTime<Utc>,
    after: Duration,
) -> Option<DateTime<Utc>> {
    if on_screen {
        return None;
    }
    let first = posts.iter().copied().filter(|p| *p > seen).min()?;
    (now - first >= after).then_some(first)
}

/// A parked item, listed by its words with Drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParkedItem {
    pub item_id: String,
    pub text: String,
}

/// A catch-up, as the chat's extras carry it (`extras.away`), beside its
/// `extras.update` (what finished and what's open, the decisions with
/// their times, and what still waits): every field but the times
/// optional on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionedAway {
    /// When the absence began and ended: the chat's "you were away"
    /// separator.
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub headline: String,
    /// The sessions that finished while they were away.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finished: Vec<String>,
    /// "I handled 23 of 25 myself, 4 by your preferences."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parked: Vec<ParkedItem>,
}

/// The catch-up's first line: what needs the person, then what finished.
pub fn headline(needs: usize, finished: &[&str]) -> String {
    let needs = match needs {
        0 => "Nothing needs you.".to_string(),
        1 => "One thing needs you.".to_string(),
        n => format!("{n} things need you."),
    };
    let finished = match finished {
        [] => String::new(),
        [one] => format!(" {one} finished."),
        [rest @ .., last] => format!(" {} and {last} finished.", rest.join(", ")),
    };
    format!("Welcome back. {needs}{finished}")
}

/// How much the boss settled itself of the needs it met, or none when it
/// met none.
pub fn handled_line(h: &Handled) -> Option<String> {
    if h.needs() == 0 {
        return None;
    }
    let by = match h.preference {
        0 => String::new(),
        p => format!(", {p} by your preferences"),
    };
    Some(format!(
        "I handled {} of {} myself{by}.",
        h.itself(),
        h.needs()
    ))
}

/// The catch-up's text, as history, Slack and the model read it: the
/// headline, the sessions, each decision (`decided`: "21:12 · what —
/// why", the row's own words) and what was kept, verbatim, how much it handled, what still
/// waits and what is parked.
pub fn render(a: &MentionedAway, u: &MentionedUpdate, decided: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    if !a.headline.is_empty() {
        out.push(a.headline.clone());
    }
    let mut middle = MentionedUpdate {
        waiting: Vec::new(),
        footer: None,
        ..u.clone()
    };
    middle.decisions.clear();
    let body = crate::update::render(&middle, &[]);
    if !body.is_empty() {
        out.push(body);
    }
    match decided {
        [] => {}
        [one] => out.push(format!("I decided: {one}")),
        many => out.push(format!(
            "I decided {} things:\n{}",
            many.len(),
            many.iter()
                .map(|d| format!("- {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
    if let Some(h) = &a.handled {
        out.push(h.clone());
    }
    let waiting = crate::update::render(
        &MentionedUpdate {
            waiting: u.waiting.clone(),
            ..Default::default()
        },
        &[],
    );
    if !waiting.is_empty() {
        out.push(waiting);
    }
    if !a.parked.is_empty() {
        out.push(format!(
            "Set aside until you say (answer or drop them):\n{}",
            a.parked
                .iter()
                .map(|p| format!("- {}", p.text))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if let Some(f) = &u.footer {
        out.push(format!("_{f}_"));
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{How, Need};
    use crate::update::{UpdateSession, WaitingItem};

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + s, 0).unwrap()
    }

    #[test]
    fn a_pause_releases_the_hold_and_three_minutes_end_it_regardless() {
        let quiet = Duration::seconds(20);
        let cap = Duration::minutes(3);
        let t = Typing::ping(None, at(0), quiet);
        assert!(t.holds(at(10), quiet, cap));
        assert!(!t.holds(at(20), quiet, cap), "20 s after the last ping");
        // Typing on keeps it held, but never past 3 minutes from the start.
        let mut t = t;
        for s in (10..=300).step_by(10) {
            t = Typing::ping(Some(t), at(s), quiet);
        }
        assert_eq!(t.first, at(0));
        assert!(t.holds(at(179), quiet, cap));
        assert!(!t.holds(at(180), quiet, cap), "the cap");
        // A ping after a pause begins a new stretch, with its own cap.
        let again = Typing::ping(Some(Typing::ping(None, at(0), quiet)), at(60), quiet);
        assert_eq!(again.first, at(60));
    }

    #[test]
    fn away_is_the_first_unread_post_once_it_has_waited_long_enough() {
        let after = Duration::minutes(15);
        let posts = [at(0), at(600), at(1200)];
        // Read through the first: the second is the first unread.
        assert_eq!(away_since(&posts, at(0), false, at(600 + 899), after), None);
        assert_eq!(
            away_since(&posts, at(0), false, at(600 + 900), after),
            Some(at(600))
        );
        // Everything read: here, however long since.
        assert_eq!(away_since(&posts, at(1200), false, at(99_999), after), None);
        // The DM on screen: here.
        assert_eq!(away_since(&posts, at(0), true, at(99_999), after), None);
        // No posts: here.
        assert_eq!(away_since(&[], at(0), false, at(99_999), after), None);
    }

    #[test]
    fn presence_counts_only_while_fresh_and_for_its_own_dm() {
        let p = Presence {
            at: at(0),
            attending: true,
            dm_on_screen: Some("desk".into()),
        };
        let fresh = Duration::minutes(3);
        assert!(p.shows("desk", at(60), fresh));
        assert!(!p.shows("other", at(60), fresh));
        assert!(!p.shows("desk", at(181), fresh), "pings stopped");
        let away = Presence {
            attending: false,
            ..p
        };
        assert!(!away.shows("desk", at(60), fresh));
    }

    #[test]
    fn the_headline_says_what_needs_you_then_what_finished() {
        assert_eq!(
            headline(0, &["Billing migration"]),
            "Welcome back. Nothing needs you. Billing migration finished."
        );
        assert_eq!(
            headline(2, &["A", "B", "C"]),
            "Welcome back. 2 things need you. A, B and C finished."
        );
        assert_eq!(headline(1, &[]), "Welcome back. One thing needs you.");
    }

    #[test]
    fn handled_counts_needs_only() {
        let mut h = Handled::default();
        assert_eq!(handled_line(&h), None);
        h.add(Need::Approval, How::Own, 19);
        h.add(Need::Question, How::Preference, 4);
        h.add(Need::Stall, How::Escalated, 2);
        h.add(Need::Choice, How::Own, 7);
        assert_eq!(
            handled_line(&h).unwrap(),
            "I handled 23 of 25 myself, 4 by your preferences."
        );
    }

    #[test]
    fn the_catch_up_reads_in_order_and_keeps_every_decision() {
        let a = MentionedAway {
            since: at(0),
            until: at(6000),
            headline: headline(1, &["Billing"]),
            finished: vec!["s1".into()],
            handled: Some("I handled 2 of 3 myself.".into()),
            parked: vec![ParkedItem {
                item_id: "p".into(),
                text: "Rename the export?".into(),
            }],
        };
        let u = MentionedUpdate {
            sessions: vec![UpdateSession {
                session_id: "s1".into(),
                name: "Billing".into(),
                purpose: "moving billing".into(),
                text: "It finished.".into(),
                refs: Vec::new(),
            }],
            waiting: vec![WaitingItem {
                item_id: "w".into(),
                text: "Add deleted leads?".into(),
                age: "2 h".into(),
                holding_up: None,
                recommendation: None,
                options: vec!["Yes".into()],
            }],
            ..Default::default()
        };
        let text = render(
            &a,
            &u,
            &[
                "21:12 · let it retry — nothing had changed".into(),
                "21:40 · not yet — you only asked for new leads".into(),
            ],
        );
        let order = [
            "Welcome back. One thing needs you. Billing finished.",
            "**Billing** (moving billing) — It finished.",
            "I decided 2 things:\n- 21:12 · let it retry",
            "- 21:40 · not yet",
            "I handled 2 of 3 myself.",
            "Still waiting on you:\n- Add deleted leads? (2 h)",
            "Set aside until you say",
            "- Rename the export?",
        ];
        let mut from = 0;
        for o in order {
            let i = text[from..]
                .find(o)
                .unwrap_or_else(|| panic!("{o} in order: {text}"));
            from += i + o.len();
        }
        // Round-trips on the wire with every optional field left out.
        let bare: MentionedAway = serde_json::from_value(serde_json::json!({
            "since": "2026-11-12T21:00:00Z", "until": "2026-11-12T22:40:00Z"
        }))
        .unwrap();
        assert!(bare.headline.is_empty() && bare.parked.is_empty());
    }
}
