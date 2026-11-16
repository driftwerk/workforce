//! The one way a session is closed. A session leaves Desk only when it is
//! finished: ended by the person, or by the agent that owns it (a boss
//! closing a session it started once its work is done, through
//! [`Desk::end`](super::Desk::end)), or a handoff's session that never took
//! its work up. Nothing else closes one, the person's least of all: not a
//! restart, not a terminal or listing that no longer shows it, not an
//! origin it is recorded with. Those keep it open, to be taken back or
//! resumed. Every other part of Desk reaches `Closed` only through
//! [`closed`].

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

    /// A line that names `Closed` without comparing or matching on it: one
    /// that would set a session to it.
    fn sets_closed(line: &str) -> bool {
        let code = line.split("//").next().unwrap_or_default();
        code.contains("SessionState::Closed")
            && !["==", "!=", "=>", "|", "matches!"]
                .iter()
                .any(|reads| code.contains(reads))
    }

    #[test]
    fn a_line_setting_closed_is_told_from_one_reading_it() {
        assert!(sets_closed("r.state = SessionState::Closed;"));
        assert!(sets_closed("    SessionState::Closed"));
        assert!(!sets_closed("if r.state == SessionState::Closed {"));
        assert!(!sets_closed(
            "SessionState::Stopped | SessionState::Closed => {"
        ));
        assert!(!sets_closed("// r.state = SessionState::Closed"));
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
            for (n, line) in outside_tests(&text).into_iter().enumerate() {
                if sets_closed(line) {
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
