//! The one way a session is closed. A session leaves Desk only when it is
//! finished: ended by the person, or by the agent that owns it (a boss
//! closing a session it started once its work is done, through
//! [`Desk::end`](super::Desk::end)), or a handoff's session that never took
//! its work up. Nothing else closes one, the person's least of all: not a
//! restart, not a terminal or listing that no longer shows it, not an
//! origin it is recorded with. Those keep it open, to be taken back or
//! resumed. Every other part of Desk reaches `Closed` only through
//! [`closed`].

use crate::charge::{Charge, How};
use crate::crew::{By, PartyKind};
use crate::model::SessionState;

/// Why a session is closed: the only reasons one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    /// Ended from Desk ([`Desk::end`](super::Desk::end)), by the person or
    /// the agent that owns it.
    Ended,
    /// Started for a handoff and never took its work up: the handoff is
    /// undone and nothing of the person's is in it.
    HandoffUntaken,
}

/// Whether `by` may end a session whose boss's charge is `charge`: the
/// person always; a boss only one it owns, one it started or the person
/// handed it, never one it took on by itself; no one else.
pub(crate) fn may_end(by: Option<&By>, charge: Option<&Charge>) -> bool {
    let Some(by) = by else {
        return false;
    };
    match by.party.kind {
        PartyKind::You => true,
        PartyKind::Manager => charge.is_some_and(|c| {
            c.live() && c.boss == by.party.id && matches!(c.how, How::Started | How::Handed)
        }),
        PartyKind::Agent | PartyKind::Session | PartyKind::Desk => false,
    }
}

/// The state a session finished for `why` is recorded in.
pub(crate) fn closed(why: Finish) -> SessionState {
    match why {
        Finish::Ended | Finish::HandoffUntaken => SessionState::Closed,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Every `.rs` file under `dir`.
    fn sources(dir: &Path) -> Vec<PathBuf> {
        let mut all = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                all.extend(sources(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                all.push(path);
            }
        }
        all
    }

    /// A source file's lines before its first inline test module.
    fn outside_tests(text: &str) -> Vec<&str> {
        let lines: Vec<&str> = text.lines().collect();
        let end = lines
            .windows(2)
            .position(|w| {
                let module = w[1].trim();
                w[0].trim() == "#[cfg(test)]"
                    && (module.starts_with("mod ") || module.starts_with("pub(crate) mod "))
                    && module.ends_with('{')
            })
            .unwrap_or(lines.len());
        lines[..end].to_vec()
    }

    /// The names `Closed` goes by in `text`: in full, and as `S::Closed`
    /// where the file calls the states `S`.
    fn names(text: &str) -> Vec<&'static str> {
        let mut names = vec!["SessionState::Closed"];
        if text.contains("SessionState as S") {
            names.push("S::Closed");
        }
        names
    }

    /// Whether `line` gives a session the state `Closed` rather than
    /// comparing or matching on it: each use of a name is read by what is
    /// either side of it.
    fn sets_closed(line: &str, names: &[&str]) -> bool {
        let code = line.split("//").next().unwrap_or_default();
        if code.contains("matches!") {
            return false;
        }
        names.iter().any(|name| {
            code.match_indices(name).any(|(at, _)| {
                let head = &code[..at];
                // A module path before it (`crate::model::`) is the same
                // name; an identifier run into it is a longer one.
                if head.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                    // The tail of a longer name.
                    return false;
                }
                let rest = &code[at + name.len()..];
                if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                    return false;
                }
                let before = head.trim_end();
                let after = rest.trim_start();
                let compared = before.ends_with("==") || before.ends_with("!=");
                let pattern = (before.ends_with('|') && !before.ends_with("||"))
                    || after.starts_with("=>")
                    || after.starts_with("if ")
                    || (after.starts_with('|') && !after.starts_with("||"))
                    || (after.starts_with(')') && after[1..].trim_start().starts_with("=>"));
                !compared && !pattern
            })
        })
    }

    #[test]
    fn only_the_person_or_the_boss_that_owns_a_session_ends_it() {
        use super::may_end;
        use crate::charge::{Charge, How};
        use crate::crew::{By, Party};
        let id = uuid::Uuid::new_v4();
        let boss = By::manager("sarah", "Sarah", "done");
        let charged = |how| Charge::new(id, "sarah", how, None);
        assert!(may_end(Some(&By::you()), None), "the person's own");
        assert!(may_end(Some(&boss), Some(&charged(How::Started))));
        assert!(may_end(Some(&boss), Some(&charged(How::Handed))));
        assert!(
            !may_end(Some(&boss), Some(&charged(How::Taken))),
            "took it on itself"
        );
        assert!(!may_end(Some(&boss), None), "never its");
        let other = Charge::new(id, "max", How::Started, None);
        assert!(!may_end(Some(&boss), Some(&other)), "another boss's");
        let mut released = charged(How::Started);
        released.released_at = Some(chrono::Utc::now());
        assert!(!may_end(Some(&boss), Some(&released)), "given back");
        let agent = By {
            party: Party::agent("scout", "Scout"),
            ..By::you()
        };
        assert!(!may_end(Some(&agent), Some(&charged(How::Started))));
        assert!(!may_end(None, None), "nothing ends one by itself");
    }

    #[test]
    fn a_line_setting_closed_is_told_from_one_reading_it() {
        let full = ["SessionState::Closed"];
        let short = ["SessionState::Closed", "S::Closed"];
        for sets in [
            "r.state = SessionState::Closed;",
            "self.change(id, |r, _| r.state = SessionState::Closed);",
            "    SessionState::Closed",
            "Some(_) => SessionState::Closed,",
            "x || y => SessionState::Closed,",
            "r.state = crate::model::SessionState::Closed;",
        ] {
            assert!(sets_closed(sets, &full), "{sets}");
        }
        assert!(sets_closed("r.state = S::Closed;", &short));
        for reads in [
            "if r.state == SessionState::Closed {",
            "if a || r.state != SessionState::Closed {",
            "SessionState::Stopped | SessionState::Closed => {",
            "SessionState::Closed if replied => Urgency::New,",
            "Some(SessionState::Closed) => {}",
            "matches!(r.state, SessionState::Stopped | SessionState::Closed)",
            "// r.state = SessionState::Closed",
            "x = SessionState::Closedish;",
            "x = MySessionState::Closed;",
        ] {
            assert!(!sets_closed(reads, &full), "{reads}");
        }
        assert!(
            !sets_closed("r.state = S::Closed;", &full),
            "no alias, no S"
        );
    }

    #[test]
    fn nothing_but_the_gate_closes_a_session() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let gate = src.join("service").join("finish.rs");
        let model = src.join("model.rs");
        let mut found = Vec::new();
        for file in sources(&src) {
            if file == gate || file == model || file.to_string_lossy().ends_with("_tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let names = names(&text);
            for (n, line) in outside_tests(&text).into_iter().enumerate() {
                if sets_closed(line, &names) {
                    found.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(
            found.is_empty(),
            "a session is closed only through finish::closed:\n{}",
            found.join("\n")
        );
    }
}
