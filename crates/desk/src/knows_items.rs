//! What an agent is sent of each row (Sees K3, `designs/agent-sees.md` §3
//! in workforce-desk): plain text fields, every string redacted, with
//! `text` the part its hub searches. Pure.

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::knows::{Held, TRANSCRIPT};
use crate::knows_collect::{AccountRow, BoardRow, DecisionRow, ExchangeRow, NeedRow};
use crate::memory::Redactor;

pub const ACCOUNT: &str = "account";
pub const DECISION: &str = "decision";
pub const NEED: &str = "need";
pub const BOARD: &str = "board";

/// The most characters of an exchange's searched text, and of its ask.
pub const EXCHANGE_TEXT: usize = 48_000;
pub const EXCHANGE_ASK: usize = 8_000;
/// The most steps, files, commands or errors an exchange carries, and the
/// most characters of each.
const LIST_ITEMS: usize = 40;
const LIST_CHARS: usize = 300;
/// The most of an account's done items.
const DONE_ITEMS: usize = 10;
/// The most characters of any other field.
const FIELD_CHARS: usize = 4_000;

/// One thing to send an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    /// What the agent's copy is recorded as here.
    pub held: Held,
    /// Its group's name, as the agent's hub keeps it.
    pub group_name: Option<String>,
    pub at: DateTime<Utc>,
    pub body: Value,
    /// Of a kind sent again when it changes, the body's hash.
    pub hash: Option<String>,
}

impl Outgoing {
    /// Its size as sent, near enough for a batch's cap.
    pub fn bytes(&self) -> usize {
        self.body.to_string().len() + 256
    }
}

/// A session's place, as the items name it.
#[derive(Debug, Clone, Default)]
pub struct Where {
    pub group: Option<String>,
    pub group_name: Option<String>,
}

fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// `path` with the home folder as `~`.
pub fn tilde(path: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty()) {
        Some(h) if path == h => "~".into(),
        Some(h) => match path.strip_prefix(h).filter(|r| r.starts_with('/')) {
            Some(rest) => format!("~{rest}"),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// Every string in `v` (keys left as they are) as it may leave this
/// computer: each secret replaced whole.
pub fn redact(v: &mut Value, redactor: &dyn Redactor) {
    match v {
        Value::String(s) => {
            let out = redactor.stored(s);
            if out.count > 0 {
                *s = out.text.into_owned();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| redact(i, redactor)),
        Value::Object(map) => map.values_mut().for_each(|i| redact(i, redactor)),
        _ => {}
    }
}

/// The hash a changed body is told by.
pub fn hash(body: &Value) -> String {
    hex::encode(Sha256::digest(body.to_string().as_bytes()))
}

/// `text` redacted whole, before any of it is cut: a secret across a cut
/// is never left half there.
fn red(r: &dyn Redactor, text: &str) -> String {
    r.stored(text).text.into_owned()
}

fn red_value(r: &dyn Redactor, v: &Value) -> Value {
    let mut v = v.clone();
    redact(&mut v, r);
    v
}

fn red_list(r: &dyn Redactor, l: &[String]) -> Vec<String> {
    l.iter().map(|t| red(r, t)).collect()
}

/// A group's name as it may be sent: redacted, one short plain line.
pub fn group_label(r: &dyn Redactor, name: &str) -> String {
    let plain: String = red(r, name)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cut(plain.trim(), 190)
}

fn finish(
    held: Held,
    place: &Where,
    at: DateTime<Utc>,
    mut body: Value,
    again: bool,
    redactor: &dyn Redactor,
) -> Outgoing {
    redact(&mut body, redactor);
    Outgoing {
        held,
        group_name: place
            .group_name
            .as_deref()
            .map(|g| group_label(redactor, g))
            .filter(|g| !g.is_empty()),
        at,
        hash: again.then(|| hash(&body)),
        body,
    }
}

fn texts(v: &Value, field: &str, max: usize) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .rev()
                .take(max)
                .rev()
                .filter_map(|i| i.get(field)?.as_str().map(|t| cut(t, LIST_CHARS)))
                .collect()
        })
        .unwrap_or_default()
}

fn lines(items: &[(&str, String)]) -> String {
    items
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A session's account.
pub fn account(
    a: &AccountRow,
    place: &Where,
    home: Option<&str>,
    redactor: &dyn Redactor,
) -> Outgoing {
    let r = redactor;
    let a = &AccountRow {
        name: red(r, &a.name),
        cwd: red(r, &a.cwd),
        purpose: red(r, &a.purpose),
        plan: red(r, &a.plan),
        now: red(r, &a.now),
        next: red(r, &a.next),
        waiting_on: a.waiting_on.as_deref().map(|w| red(r, w)),
        done: red_value(r, &a.done),
        decisions: red_value(r, &a.decisions),
        ..a.clone()
    };
    let done = texts(&a.done, "text", DONE_ITEMS);
    let decisions: Vec<Value> = a
        .decisions
        .as_array()
        .map(|d| {
            d.iter()
                .rev()
                .take(DONE_ITEMS)
                .rev()
                .map(|x| {
                    json!({
                        "what": cut(x.get("what").and_then(Value::as_str).unwrap_or(""), LIST_CHARS),
                        "why": cut(x.get("why").and_then(Value::as_str).unwrap_or(""), LIST_CHARS),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let waiting = a.waiting_on.clone().unwrap_or_default();
    let text = lines(&[
        ("Session", a.name.clone()),
        ("Purpose", cut(&a.purpose, FIELD_CHARS)),
        ("Plan", cut(&a.plan, FIELD_CHARS)),
        ("Now", cut(&a.now, FIELD_CHARS)),
        ("Next", cut(&a.next, FIELD_CHARS)),
        ("Waiting on", cut(&waiting, FIELD_CHARS)),
        ("Done", done.join("; ")),
    ]);
    let body = json!({
        "name": cut(&a.name, 200),
        "engine": a.engine,
        "folder": cut(&tilde(&a.cwd, home), 500),
        "group": place.group_name,
        "state": a.state,
        "purpose": cut(&a.purpose, FIELD_CHARS),
        "plan": cut(&a.plan, FIELD_CHARS),
        "now": cut(&a.now, FIELD_CHARS),
        "next": cut(&a.next, FIELD_CHARS),
        "waiting_on": a.waiting_on.as_deref().map(|w| cut(w, FIELD_CHARS)),
        "done": done,
        "decisions": decisions,
        "written_at": a.written_at,
        "text": text,
    });
    finish(
        Held {
            kind: ACCOUNT.into(),
            reference: a.session.to_string(),
            session: Some(a.session),
            group: place.group.clone(),
            board: None,
            boss: None,
        },
        place,
        a.written_at,
        body,
        true,
        redactor,
    )
}

/// A boss's question or confirmation for the person.
pub fn need(n: &NeedRow, boss_name: &str, place: &Where, redactor: &dyn Redactor) -> Outgoing {
    let r = redactor;
    let n = &NeedRow {
        session_name: n.session_name.as_deref().map(|x| red(r, x)),
        text: red(r, &n.text),
        holding_up: n.holding_up.as_deref().map(|x| red(r, x)),
        ..n.clone()
    };
    let body = json!({
        "boss": boss_name,
        "kind": n.kind,
        "session": n.session_name,
        "what": cut(&n.text, FIELD_CHARS),
        "holding_up": n.holding_up.as_deref().map(|h| cut(h, FIELD_CHARS)),
        "since": n.opened_at,
        "state": n.state,
        "text": cut(&n.text, FIELD_CHARS),
    });
    finish(
        Held {
            kind: NEED.into(),
            reference: n.id.to_string(),
            session: n.session,
            group: place.group.clone(),
            board: None,
            boss: Some(n.boss.clone()),
        },
        place,
        n.changed_at,
        body,
        true,
        redactor,
    )
}

/// A boss's decision.
pub fn decision(
    d: &DecisionRow,
    boss_name: &str,
    place: &Where,
    redactor: &dyn Redactor,
) -> Outgoing {
    let r = redactor;
    let d = &DecisionRow {
        session_name: d.session_name.as_deref().map(|x| red(r, x)),
        need: cut(&d.need, 40),
        what: red(r, &d.what),
        why: red(r, &d.why),
        how: cut(&d.how, 40),
        ..d.clone()
    };
    let text = lines(&[
        ("Decided", cut(&d.what, FIELD_CHARS)),
        ("Why", cut(&d.why, FIELD_CHARS)),
    ]);
    let body = json!({
        "boss": boss_name,
        "session": d.session_name,
        "need": d.need,
        "what": cut(&d.what, FIELD_CHARS),
        "why": cut(&d.why, FIELD_CHARS),
        "how": d.how,
        "at": d.at,
        "undone_at": d.undone_at,
        "text": text,
    });
    finish(
        Held {
            kind: DECISION.into(),
            reference: d.id.to_string(),
            session: d.session,
            group: place.group.clone(),
            board: None,
            boss: Some(d.boss.clone()),
        },
        place,
        d.undone_at.unwrap_or(d.at),
        body,
        true,
        redactor,
    )
}

/// Something that happened on a board's ticket.
pub fn board(b: &BoardRow, place: &Where, redactor: &dyn Redactor) -> Outgoing {
    let r = redactor;
    let b = &BoardRow {
        by: red(r, &b.by),
        text: red(r, &b.text),
        board_name: red(r, &b.board_name),
        title: red(r, &b.title),
        ..b.clone()
    };
    let body = json!({
        "board": cut(&b.board_name, 200),
        "ticket": b.ticket,
        "title": cut(&b.title, 500),
        "what": b.kind,
        "by": cut(&b.by, 200),
        "at": b.at,
        "text": lines(&[
            ("Ticket", format!("{} {}", b.ticket, cut(&b.title, 500))),
            ("What", cut(&b.text, FIELD_CHARS)),
        ]),
    });
    finish(
        Held {
            kind: BOARD.into(),
            reference: b.id.clone(),
            session: None,
            group: b.group.clone(),
            board: Some(b.board),
            boss: None,
        },
        place,
        b.at,
        body,
        false,
        redactor,
    )
}

fn list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .take(LIST_ITEMS)
        .map(|i| cut(i, LIST_CHARS))
        .collect()
}

/// A closed exchange of a session.
pub fn exchange(e: &ExchangeRow, place: &Where, redactor: &dyn Redactor) -> Outgoing {
    let r = redactor;
    let e = &ExchangeRow {
        session_name: e.session_name.as_deref().map(|x| red(r, x)),
        ask: red(r, &e.ask),
        answer: red(r, &e.answer),
        steps: red_value(r, &e.steps),
        files: red_list(r, &e.files),
        commands: red_list(r, &e.commands),
        errors: red_list(r, &e.errors),
        flagged: e
            .flagged
            .iter()
            .take(LIST_ITEMS)
            .map(|f| cut(f, 60))
            .collect(),
        ..e.clone()
    };
    let steps: Vec<Value> = e
        .steps
        .as_array()
        .map(|s| {
            s.iter()
                .take(LIST_ITEMS)
                .map(|x| {
                    let mut m = Map::new();
                    for f in ["tool", "input"] {
                        if let Some(t) = x.get(f).and_then(Value::as_str) {
                            m.insert(f.into(), Value::String(cut(t, LIST_CHARS)));
                        }
                    }
                    if x.get("failed").and_then(Value::as_bool) == Some(true) {
                        m.insert("failed".into(), Value::Bool(true));
                    }
                    Value::Object(m)
                })
                .collect()
        })
        .unwrap_or_default();
    let body = json!({
        "session": e.session_name,
        "at": e.at,
        "ended": e.ended,
        "asker": e.asker,
        "ask": cut(&e.ask, EXCHANGE_ASK),
        "steps": steps,
        "files": list(&e.files),
        "commands": list(&e.commands),
        "errors": list(&e.errors),
        "flagged": e.flagged,
        "text": cut(&format!("{}\n\n{}", e.ask, e.answer), EXCHANGE_TEXT),
    });
    finish(
        Held {
            kind: TRANSCRIPT.into(),
            reference: e.reference.clone(),
            session: Some(e.session),
            group: e.group.clone(),
            board: None,
            boss: None,
        },
        place,
        e.ended,
        body,
        false,
        redactor,
    )
}

/// A session's id, as an item that isn't of one names none.
pub fn session_of(o: &Outgoing) -> Option<Uuid> {
    o.held.session
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::memory::Redacted;

    /// Hides anything after `secret=`.
    struct Hide;
    impl Redactor for Hide {
        fn stored<'a>(&self, text: &'a str) -> Redacted<'a> {
            match text.find("secret=") {
                Some(i) => Redacted {
                    text: Cow::Owned(format!("{}[REDACTED]", &text[..i])),
                    count: 1,
                },
                None => Redacted {
                    text: Cow::Borrowed(text),
                    count: 0,
                },
            }
        }
    }

    fn at() -> DateTime<Utc> {
        "2026-11-13T10:00:00Z".parse().unwrap()
    }

    #[test]
    fn every_string_of_every_kind_is_redacted_and_home_is_a_tilde() {
        let s = Uuid::from_u128(1);
        let place = Where {
            group: Some("g1".into()),
            group_name: Some("Patchwork secret=abc".into()),
        };
        let a = account(
            &AccountRow {
                session: s,
                name: "atlas secret=n".into(),
                engine: "claude".into(),
                cwd: "/Users/me/dev/atlas".into(),
                state: "working".into(),
                purpose: "ship secret=p".into(),
                plan: String::new(),
                now: "now".into(),
                next: String::new(),
                waiting_on: Some("you secret=w".into()),
                done: json!([{"text": "did secret=d"}]),
                decisions: json!([{"what": "w secret=x", "why": ""}]),
                written_at: at(),
            },
            &place,
            Some("/Users/me"),
            &Hide,
        );
        let e = exchange(
            &ExchangeRow {
                reference: "claude:k".into(),
                session: s,
                session_name: Some("atlas".into()),
                group: Some("g1".into()),
                at: at(),
                ended: at(),
                asker: "person".into(),
                ask: "use secret=a".into(),
                answer: "ok".into(),
                steps: json!([{"tool": "Bash", "input": "export secret=s", "result": "never sent"}]),
                files: vec!["f secret=f".into()],
                commands: vec![],
                errors: vec![],
                flagged: vec![],
            },
            &place,
            &Hide,
        );
        let n = need(
            &NeedRow {
                id: Uuid::from_u128(2),
                boss: "allan".into(),
                kind: "ask".into(),
                session: Some(s),
                sessions: vec![s],
                session_name: Some("atlas secret=q".into()),
                text: "merge? secret=t".into(),
                holding_up: None,
                state: "open".into(),
                opened_at: at(),
                changed_at: at(),
            },
            "Allan",
            &place,
            &Hide,
        );
        let d = decision(
            &DecisionRow {
                id: Uuid::from_u128(3),
                boss: "allan".into(),
                session: Some(s),
                session_name: Some("atlas".into()),
                need: "approval".into(),
                what: "ran secret=r".into(),
                why: "why secret=y".into(),
                how: "own".into(),
                at: at(),
                undone_at: None,
            },
            "Allan",
            &place,
            &Hide,
        );
        let b = board(
            &BoardRow {
                id: "act-1".into(),
                at: at(),
                kind: "ticket.moved".into(),
                by: "Mat".into(),
                text: "moved secret=m".into(),
                board: Uuid::from_u128(4),
                board_name: "Atlas secret=b".into(),
                group: Some("g1".into()),
                ticket: "AT-1".into(),
                title: "t secret=tt".into(),
            },
            &place,
            &Hide,
        );
        for o in [&a, &e, &n, &d, &b] {
            let text = o.body.to_string();
            assert!(!text.contains("secret="), "{text}");
            assert!(text.contains("[REDACTED]"), "{text}");
        }
        assert_eq!(a.body["folder"], "~/dev/atlas");
        // What a step returned is never sent.
        assert!(!e.body.to_string().contains("never sent"));
        // Sent again when changed: accounts, needs, decisions.
        assert!(a.hash.is_some() && n.hash.is_some() && d.hash.is_some());
        assert!(e.hash.is_none() && b.hash.is_none());
        assert_eq!(e.held.reference, "claude:k");
        assert_eq!(b.held.board, Some(Uuid::from_u128(4)));
    }

    #[test]
    fn a_secret_across_a_cut_is_redacted_whole_first() {
        let place = Where::default();
        let a = account(
            &AccountRow {
                session: Uuid::from_u128(1),
                name: "n".into(),
                engine: "claude".into(),
                cwd: "/x".into(),
                state: "working".into(),
                purpose: format!("{}secret=abcdefgh", "x".repeat(FIELD_CHARS - 3)),
                plan: String::new(),
                now: String::new(),
                next: String::new(),
                waiting_on: None,
                done: json!([]),
                decisions: json!([]),
                written_at: at(),
            },
            &place,
            None,
            &Hide,
        );
        let text = a.body.to_string();
        assert!(!text.contains("sec"), "{}", &text[..200]);
        assert_eq!(
            group_label(&Hide, "Patch\nwork secret=1"),
            "Patch work [REDACTED]"
        );
    }

    #[test]
    fn long_text_is_cut_on_a_character() {
        assert_eq!(cut("héllo", 2), "hé…");
        assert_eq!(cut("hi", 2), "hi");
        assert_eq!(tilde("/Users/meow/x", Some("/Users/me")), "/Users/meow/x");
        assert_eq!(tilde("/Users/me", Some("/Users/me")), "~");
    }
}
