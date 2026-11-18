//! Finding tickets by what their board knows of them: their column, and on
//! a synced board what their issue says (its labels, type, epic, who has
//! it there and when it was made). A boss finds the tickets a plan takes
//! on with it ("the recent bugs on Mantis"); the board's own filters show
//! the same facts.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::board::{Board, Ticket, TicketState};
use crate::crew::PartyKind;

/// The most tickets one search gives.
pub const FIND_MAX: usize = 100;

/// What a search asks for: every one said must hold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketFind {
    /// A board by its name or its prefix (`MAN`), any case.
    #[serde(default)]
    pub board: Option<String>,
    /// Its columns; none is every open one (Backlog, To do, Doing, Review).
    #[serde(default)]
    pub states: Vec<TicketState>,
    /// A label its issue has (Linear's labels, Jira's), any case.
    #[serde(default)]
    pub label: Option<String>,
    /// Its issue's type there (Bug, Story, Task), any case.
    #[serde(default)]
    pub kind: Option<String>,
    /// The epic, or the parent, its issue is under: by its key or its
    /// title, any case.
    #[serde(default)]
    pub epic: Option<String>,
    /// Made at or after: its issue's own date where it has one.
    #[serde(default)]
    pub created_since: Option<DateTime<Utc>>,
    /// Who has it: `me` (the person, here or there), `none` (no one, here
    /// or there), or a name, any part of it, any case.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Words in its title or its key, any case.
    #[serde(default)]
    pub q: Option<String>,
}

/// Whether `b` is the board `named` (its name or prefix, any case).
pub fn board_named(b: &Board, named: &str) -> bool {
    let n = named.trim();
    b.name.eq_ignore_ascii_case(n)
        || b.prefix
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(n))
}

/// When `t` was made, as its issue says, else as Desk does.
pub fn created(t: &Ticket) -> DateTime<Utc> {
    t.external
        .as_ref()
        .and_then(|e| e.details.facts.created)
        .unwrap_or(t.created_at)
}

/// Who has `t`'s issue there, by name, when it is synced and someone does.
pub fn assignee_there(t: &Ticket) -> Option<&str> {
    t.external
        .as_ref()
        .and_then(|e| e.assignee.as_ref())
        .and_then(Option::as_ref)
        .map(|a| a.name.as_str())
}

/// Whether `t`, keyed `key`, is one `f` asks for; `me` is who the person
/// is in its board's tracker, when it is synced and that is known.
pub fn matches(f: &TicketFind, me: Option<&str>, key: Option<&str>, t: &Ticket) -> bool {
    let has = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let contains =
        |hay: &str, needle: &str| hay.to_lowercase().contains(&needle.trim().to_lowercase());
    let open = matches!(
        t.state,
        TicketState::Backlog | TicketState::Todo | TicketState::Doing | TicketState::Review
    );
    let state = match f.states.is_empty() {
        true => open,
        false => f.states.contains(&t.state),
    };
    let details = t.external.as_ref().map(|e| &e.details);
    let label = f
        .label
        .as_deref()
        .is_none_or(|want| details.is_some_and(|d| d.labels.iter().any(|l| has(l, want))));
    let kind = f.kind.as_deref().is_none_or(|want| {
        details
            .and_then(|d| d.facts.kind.as_deref())
            .is_some_and(|k| has(k, want))
    });
    let epic = f.epic.as_deref().is_none_or(|want| {
        details.is_some_and(|d| {
            [d.facts.epic.as_ref(), d.facts.parent.as_ref()]
                .into_iter()
                .flatten()
                .any(|r| has(&r.key, want) || has(&r.title, want))
        })
    });
    let since = f.created_since.is_none_or(|at| created(t) >= at);
    let me = || {
        t.assignee
            .as_ref()
            .is_some_and(|a| a.kind == PartyKind::You)
            || match (me, t.external.as_ref().and_then(|e| e.assignee.as_ref())) {
                (Some(me), Some(Some(a))) => me == a.id,
                _ => false,
            }
    };
    let assignee = match f.assignee.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(w) if w.eq_ignore_ascii_case("me") => me(),
        Some(w) if w.eq_ignore_ascii_case("none") => {
            t.assignee.is_none() && assignee_there(t).is_none()
        }
        Some(w) => {
            assignee_there(t).is_some_and(|n| contains(n, w))
                || t.assignee.as_ref().is_some_and(|a| contains(&a.name, w))
        }
    };
    let words =
        f.q.as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .is_none_or(|q| contains(&t.title, q) || key.is_some_and(|k| has(k, q)));
    state && label && kind && epic && since && assignee && words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Assignee, External, IssueRef, System};
    use crate::crew::Party;
    use uuid::Uuid;

    fn board() -> Board {
        let at: DateTime<Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        Board {
            id: Uuid::nil(),
            name: "Mantis".into(),
            group: None,
            prefix: Some("MAN".into()),
            sync: None,
            next_number: 1,
            created_at: at,
            updated_at: at,
        }
    }

    fn issue(
        kind: &str,
        labels: &[&str],
        epic: Option<&str>,
        who: Option<&str>,
        made: &str,
    ) -> Ticket {
        let at: DateTime<Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let mut e: External = serde_json::from_value(serde_json::json!({
            "system": "linear", "key": "MAN-465", "url": "https://linear.app/x/issue/MAN-465",
            "remoteUpdatedAt": "2026-11-10T00:00:00Z", "synced": true, "link": Uuid::nil(),
        }))
        .unwrap();
        e.system = System::Linear;
        e.details.labels = labels.iter().map(|l| l.to_string()).collect();
        e.details.facts.kind = Some(kind.into());
        e.details.facts.epic = epic.map(|k| IssueRef {
            key: k.into(),
            title: "Checkout".into(),
            ..Default::default()
        });
        e.details.facts.created = Some(made.parse().unwrap());
        e.assignee = Some(who.map(|w| Assignee {
            id: format!("u-{w}"),
            name: w.into(),
        }));
        Ticket {
            id: Uuid::new_v4(),
            board: Uuid::nil(),
            number: 1,
            title: "The cart empties on reload".into(),
            body: String::new(),
            state: TicketState::Todo,
            rank: 1.0,
            upstream_rank: None,
            assignee: None,
            needs: vec![],
            links: vec![],
            external: Some(e),
            created_by: Party::you(),
            pinned: false,
            snoozed_until: None,
            snoozed_at: None,
            state_at: at,
            created_at: at,
            updated_at: at,
            version: 0,
        }
    }

    #[test]
    fn the_recent_bugs_are_found_by_what_their_issues_say() {
        let b = board();
        let bug = issue(
            "Bug",
            &["Bug", "web"],
            Some("MAN-400"),
            Some("Kim"),
            "2026-11-12T09:00:00Z",
        );
        let find = |f: TicketFind| matches(&f, Some("u-Kim"), Some("MAN-465"), &bug);
        assert!(find(TicketFind::default()));
        assert!(find(TicketFind {
            board: None,
            label: Some("bug".into()),
            created_since: Some("2026-11-10T00:00:00Z".parse().unwrap()),
            ..Default::default()
        }));
        assert!(!find(TicketFind {
            created_since: Some("2026-11-13T00:00:00Z".parse().unwrap()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            kind: Some("bug".into()),
            ..Default::default()
        }));
        assert!(!find(TicketFind {
            kind: Some("Story".into()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            epic: Some("man-400".into()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            epic: Some("checkout".into()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            assignee: Some("me".into()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            assignee: Some("ki".into()),
            ..Default::default()
        }));
        assert!(!find(TicketFind {
            assignee: Some("none".into()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            q: Some("cart".into()),
            ..Default::default()
        }));
        assert!(find(TicketFind {
            q: Some("man-465".into()),
            ..Default::default()
        }));
        assert!(!find(TicketFind {
            states: vec![TicketState::Done],
            ..Default::default()
        }));
        let mut done = bug.clone();
        done.state = TicketState::Done;
        assert!(
            !matches(&TicketFind::default(), None, None, &done),
            "open ones unless asked"
        );
        let mut unknown = bug.clone();
        unknown.external.as_mut().unwrap().assignee = Some(None);
        let none = TicketFind {
            assignee: Some("none".into()),
            ..Default::default()
        };
        assert!(matches(&none, None, None, &unknown));
        assert!(board_named(&b, "man") && board_named(&b, "mantis") && !board_named(&b, "acme"));
    }
}
