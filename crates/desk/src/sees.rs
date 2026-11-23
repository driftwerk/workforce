//! What a boss served from a grant may see of the group's sessions, by the
//! grant's Sees (remote-bosses design §8.2): what each part of a crew
//! reading is, and whether it leaves this engine. Pure; the hub's served
//! readings ask it of every part they put in, and redact and cap what
//! goes.

use crate::grant::Sees;
use crate::sight::{Glance, OpenCall, PullGlance, Reading, WorkspaceGlance};
use crate::{ProcessRow, ProcessTree, Processes};

/// A part of what a crew reading says of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Its name, state, folder, goal, since when, who runs it.
    Status,
    /// That it asks something, and what: the person's or the boss's to
    /// answer.
    Question,
    /// Its last prompt and last reply.
    Reply,
    /// Its terminal's text.
    Terminal,
    /// What Desk sees inside it: its transcript, processes, workspace and
    /// logs in full.
    Inside,
    /// The activity's reasons and texts.
    Words,
}

/// Whether `part` is shown at `sees`.
pub fn shows(sees: Sees, part: Part) -> bool {
    match part {
        Part::Status => true,
        Part::Question | Part::Reply | Part::Words => {
            matches!(sees, Sees::Replies | Sees::Everything)
        }
        Part::Terminal | Part::Inside => sees == Sees::Everything,
    }
}

/// The most characters one served result's text may hold.
pub const MAX_CHARS: usize = 16_000;
/// The most terminal rows a served result may hold.
pub const MAX_ROWS: usize = 200;

/// `text` cut to its last `MAX_ROWS` rows.
pub fn last_rows(text: &str) -> String {
    let rows: Vec<&str> = text.lines().collect();
    rows[rows.len().saturating_sub(MAX_ROWS)..].join("\n")
}

/// `text` cut to `max` characters, said so.
pub fn capped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(40)).collect();
    format!("{kept}\n[cut: more than {max} characters]")
}

/// `r` with its words and what it was read from left out, short of
/// [`Part::Reply`]: its kind, wait and since stay.
fn reading(r: Reading, sees: Sees) -> Reading {
    match shows(sees, Part::Reply) {
        true => r,
        false => Reading {
            words: String::new(),
            by: None,
            on: None,
            ..r
        },
    }
}

/// What a glance says at `sees`: at Status the reading's kind, wait and
/// durations, the folder's counts and CI's overall state; at Replies its
/// words too, the open call's tool, the branch and the PR's number; at
/// Everything all of it.
pub fn glance(g: Glance, sees: Sees) -> Glance {
    if shows(sees, Part::Inside) {
        return g;
    }
    let replies = shows(sees, Part::Reply);
    Glance {
        reading: reading(g.reading, sees),
        open_call: g.open_call.filter(|_| replies).map(|c| OpenCall {
            input: String::new(),
            ..c
        }),
        output: None,
        workspace: g.workspace.map(|w| WorkspaceGlance {
            branch: w.branch.filter(|_| replies),
            pr: w.pr.map(|p| PullGlance {
                number: if replies { p.number } else { 0 },
                failed: Vec::new(),
                ..p
            }),
            note: w.note.filter(|_| replies),
            ..w
        }),
        stall: None,
        ..g
    }
}

/// What a process listing says at `sees`: at Status the reading's kind,
/// wait and durations and each process's state and times; at Replies the
/// reading's words and the programs too; at Everything their arguments.
pub fn processes(p: Processes, sees: Sees) -> Processes {
    let Processes::Tree(t) = p else {
        return p;
    };
    if shows(sees, Part::Inside) {
        return Processes::Tree(t);
    }
    let replies = shows(sees, Part::Reply);
    Processes::Tree(ProcessTree {
        reading: reading(t.reading, sees),
        rows: t
            .rows
            .into_iter()
            .map(|r| ProcessRow {
                program: if replies { r.program } else { String::new() },
                args: None,
                ..r
            })
            .collect(),
        ..t
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_shows_only_status_and_everything_alone_shows_the_terminal() {
        for p in [
            Part::Question,
            Part::Reply,
            Part::Words,
            Part::Terminal,
            Part::Inside,
        ] {
            assert!(!shows(Sees::Status, p), "{p:?}");
        }
        assert!(shows(Sees::Status, Part::Status));
        assert!(shows(Sees::Replies, Part::Reply));
        assert!(!shows(Sees::Replies, Part::Terminal));
        assert!(shows(Sees::Everything, Part::Terminal));
        assert!(!shows(Sees::Replies, Part::Inside));
        assert!(shows(Sees::Everything, Part::Inside));
    }

    #[test]
    fn terminals_and_texts_are_capped() {
        let t: String = (0..500).map(|i| format!("row {i}\n")).collect();
        let kept = last_rows(&t);
        assert_eq!(kept.lines().count(), MAX_ROWS);
        assert!(kept.ends_with("row 499"));
        let long = "x".repeat(MAX_CHARS + 10);
        let c = capped(&long, MAX_CHARS);
        assert!(c.chars().count() <= MAX_CHARS + 1);
        assert!(c.contains("[cut:"));
        assert_eq!(capped("short", MAX_CHARS), "short");
    }

    fn seen() -> Glance {
        use crate::sight::{Growth, Kind, Wait};
        let at = chrono::DateTime::from_timestamp(1_794_000_000, 0).unwrap();
        Glance {
            reading: Reading {
                kind: Kind::Waits(Wait::Queue),
                words: "waiting for a build slot".into(),
                since: at,
                by: Some("ssh".into()),
                on: Some("devbox".into()),
                pid: None,
                call: None,
            },
            open_call: Some(OpenCall {
                id: "t1".into(),
                tool: "Bash".into(),
                input: "~/bin/rbuild w -- cargo test".into(),
                since: at,
            }),
            transcript: Some(Growth {
                grew_at: at,
                records: 2,
            }),
            cpu_active_at: Some(at),
            output_at: Some(at),
            output: Some("rbuild: all 3 build slots are busy".into()),
            background: 0,
            settled: true,
            since: at,
            as_of: Some(at),
            aged: false,
            workspace: Some(WorkspaceGlance {
                branch: Some("dw-1".into()),
                unpushed: Some(2),
                uncommitted: 1,
                behind: Some(0),
                left: None,
                pr: Some(PullGlance {
                    number: 12,
                    checks: "failure".into(),
                    failed: vec!["test".into()],
                    head_matches_local: Some(true),
                }),
                note: Some("Its PR on acme/web is read as the person.".into()),
            }),
            stall: None,
        }
    }

    #[test]
    fn a_glance_at_status_keeps_kinds_durations_counts_and_cis_state_only() {
        let g = glance(seen(), Sees::Status);
        assert_eq!(g.reading.kind, seen().reading.kind);
        assert_eq!(g.reading.since, seen().reading.since);
        assert!(g.reading.words.is_empty() && g.reading.by.is_none() && g.reading.on.is_none());
        assert_eq!((g.open_call, g.output), (None, None));
        let w = g.workspace.unwrap();
        assert_eq!((w.branch, w.unpushed, w.uncommitted), (None, Some(2), 1));
        let pr = w.pr.unwrap();
        assert_eq!(
            (pr.number, pr.checks.as_str(), pr.failed.len()),
            (0, "failure", 0)
        );
        assert_eq!(w.note, None, "nothing naming its repository");
        assert_eq!(g.transcript, seen().transcript);
    }

    #[test]
    fn a_pr_at_status_is_said_without_a_number_or_checks_it_doesnt_show() {
        let now = chrono::Utc::now();
        let g = glance(seen(), Sees::Status);
        let line = g.render(chrono_tz::UTC, now);
        assert!(line.contains("PR: CI failure"), "{line}");
        assert!(
            !line.contains("PR #") && !line.contains("check named"),
            "{line}"
        );
        let t = crate::sight::Thresholds {
            quiet: chrono::Duration::zero(),
            stall: chrono::Duration::zero(),
        };
        let s = crate::sight::verdict(&g, crate::model::SessionState::Done, now, t).unwrap();
        assert_eq!(s.words, "its PR's CI failed");
    }

    #[test]
    fn a_glance_at_replies_adds_words_the_calls_tool_the_branch_and_the_pr() {
        let g = glance(seen(), Sees::Replies);
        assert_eq!(g.reading, seen().reading);
        let call = g.open_call.unwrap();
        assert_eq!((call.tool.as_str(), call.input.as_str()), ("Bash", ""));
        assert_eq!(g.output, None);
        let w = g.workspace.unwrap();
        assert_eq!(w.branch.as_deref(), Some("dw-1"));
        assert_eq!(w.pr.unwrap().number, 12);
        assert_eq!(glance(seen(), Sees::Everything), seen());
    }

    #[test]
    fn processes_show_programs_at_replies_and_arguments_at_everything_alone() {
        let row = ProcessRow {
            pid: 2,
            ppid: 1,
            depth: 1,
            program: "ssh".into(),
            args: Some("ssh devbox ~/bin/rbuild-run w cargo test".into()),
            state: "S".into(),
            ran: "2 h".into(),
            cpu: "0:01".into(),
            cpu_growing: false,
            rss: "4 MB".into(),
        };
        let tree = Processes::Tree(ProcessTree {
            root: 1,
            reading: seen().reading,
            rows: vec![row],
            more: 0,
            as_of: seen().since,
        });
        let rows = |p: Processes| match p {
            Processes::Tree(t) => (
                t.reading.words,
                t.rows[0].program.clone(),
                t.rows[0].args.clone(),
            ),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            rows(processes(tree.clone(), Sees::Status)),
            (String::new(), String::new(), None)
        );
        let (words, program, args) = rows(processes(tree.clone(), Sees::Replies));
        assert_eq!(
            (words.as_str(), program.as_str(), args),
            ("waiting for a build slot", "ssh", None)
        );
        assert_eq!(processes(tree.clone(), Sees::Everything), tree);
        assert_eq!(
            processes(Processes::Unlisted, Sees::Status),
            Processes::Unlisted
        );
    }
}
