//! What Desk sees inside a session from evidence, without asking it: what
//! runs under it and what that waits on, read by Desk's own background
//! pass ([`crate::service::Desk::sight_pass`]). Pure; the reading is
//! sampled by the service.

pub mod logs;
pub mod procs;
pub mod transcript;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::memory::record::Part;

/// What a wait waits on, in fixed words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wait {
    Queue,
    Lock,
    Retry,
}

/// What a session's evidence reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "wait", rename_all = "snake_case")]
pub enum Kind {
    Waits(Wait),
    Works,
    Watches,
    Remote,
    Input,
    Quiet,
}

impl Kind {
    /// Whether it holds the session up on something other than its own
    /// work: a wait, its terminal, or a command on another computer.
    pub fn holds_up(self) -> bool {
        matches!(self, Self::Waits(_) | Self::Input | Self::Remote)
    }
}

/// What the evidence says a session is doing now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    #[serde(flatten)]
    pub kind: Kind,
    /// In words taken from what the session runs or prints.
    pub words: String,
    pub since: DateTime<Utc>,
    /// The process or call it was read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The computer a remote command runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// The process it was read from.
    #[serde(skip)]
    pub pid: Option<u32>,
    /// The open call it was read from: the waiting call's id.
    #[serde(skip)]
    pub call: Option<String>,
}

impl Reading {
    /// Nothing seen running at `now`.
    pub fn quiet(now: DateTime<Utc>) -> Self {
        Self {
            kind: Kind::Quiet,
            words: String::new(),
            since: now,
            by: None,
            on: None,
            pid: None,
            call: None,
        }
    }
}

/// A tool call a session's record shows started with nothing back yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCall {
    pub id: String,
    pub tool: String,
    pub input: String,
    pub since: DateTime<Utc>,
}

/// The newest call still open at the end of `lines`, each a record line's
/// time and parts, oldest first: a call is closed by its result, and every
/// call by the turn's end or the next prompt.
pub fn open_call(lines: impl IntoIterator<Item = (DateTime<Utc>, Vec<Part>)>) -> Option<OpenCall> {
    lines
        .into_iter()
        .fold(Vec::<OpenCall>::new(), |mut open, (at, parts)| {
            parts.into_iter().for_each(|part| match part {
                Part::Call {
                    id, tool, input, ..
                } => open.push(OpenCall {
                    id,
                    tool,
                    input,
                    since: at,
                }),
                Part::Result { id, .. } => open.retain(|c| c.id != id),
                Part::Ask { .. } | Part::End { .. } | Part::Compacted { .. } => open.clear(),
                Part::Text(_) => {}
            });
            open
        })
        .pop()
}

/// `d` in a few words: "45 s", "12 min", "2 h 50 min", "3 d 4 h".
pub fn lasted(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    let (days, hours, mins) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs} s"),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::Asker;

    fn at(min: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_794_000_000, 0).unwrap() + Duration::minutes(min)
    }

    fn call(id: &str) -> Part {
        Part::Call {
            id: id.into(),
            tool: "Bash".into(),
            input: format!("run {id}"),
            files: vec![],
            command: None,
        }
    }

    fn result(id: &str) -> Part {
        Part::Result {
            id: id.into(),
            text: "ok".into(),
            failed: false,
        }
    }

    #[test]
    fn the_newest_call_with_no_result_is_the_open_one() {
        let open = open_call([
            (at(0), vec![call("a"), call("b")]),
            (at(1), vec![result("b")]),
        ])
        .unwrap();
        assert_eq!((open.id.as_str(), open.since), ("a", at(0)));
        assert_eq!(open.input, "run a");
        assert_eq!(
            open_call([(at(0), vec![call("a")]), (at(1), vec![result("a")])]),
            None
        );
    }

    #[test]
    fn a_turns_end_or_a_new_prompt_closes_every_call() {
        assert_eq!(
            open_call([
                (at(0), vec![call("a")]),
                (at(1), vec![Part::End { at: at(1) }])
            ]),
            None
        );
        let ask = Part::Ask {
            key: "k".into(),
            at: at(2),
            asker: Asker::Person,
            text: "next".into(),
        };
        assert_eq!(
            open_call([(at(0), vec![call("a")]), (at(2), vec![ask])]),
            None
        );
        assert_eq!(open_call(Vec::new()), None);
    }

    #[test]
    fn a_duration_is_said_in_its_largest_units() {
        assert_eq!(lasted(Duration::seconds(45)), "45 s");
        assert_eq!(lasted(Duration::minutes(12)), "12 min");
        assert_eq!(lasted(Duration::minutes(170)), "2 h 50 min");
        assert_eq!(lasted(Duration::hours(3)), "3 h");
        assert_eq!(lasted(Duration::hours(76)), "3 d 4 h");
        assert_eq!(lasted(Duration::days(2)), "2 d");
        assert_eq!(lasted(Duration::seconds(-5)), "0 s");
    }

    #[test]
    fn a_reading_is_written_with_its_kind_and_wait_beside_its_words() {
        let r = Reading {
            kind: Kind::Waits(Wait::Queue),
            words: "waiting for a build slot".into(),
            since: at(0),
            by: Some("ssh".into()),
            on: None,
            pid: Some(7),
            call: Some("toolu_1".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "waits");
        assert_eq!(v["wait"], "queue");
        assert!(v.get("pid").is_none() && v.get("call").is_none());
        let back: Reading = serde_json::from_value(v).unwrap();
        assert_eq!(
            back,
            Reading {
                pid: None,
                call: None,
                ..r
            }
        );
        let works = serde_json::to_value(Reading::quiet(at(0))).unwrap();
        assert_eq!(works["kind"], "quiet");
        assert!(works.get("wait").is_none());
    }
}
