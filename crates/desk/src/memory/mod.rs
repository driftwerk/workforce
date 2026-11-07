//! Session memory: what each session said and did, harvested from its
//! engine's own record by the ledger's reader, redacted before it is kept,
//! and never harvested from what is the hub's own or was forgotten. The
//! shapes and rules here are pure; the desk's I/O shell reads, stores and
//! searches.

pub mod harvest;
pub mod record;

use std::collections::HashSet;
use std::path::Path;

use crate::adopt::HubInternal;
use crate::engine::Engine;

/// Text as it may be kept: every secret in it replaced whole, and how many
/// were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub count: u32,
}

/// Hides secrets in what is kept, with the hub's patterns; the desk can't
/// read them. Each secret is replaced in full by its label, with no part of
/// it kept.
pub trait Redactor: Send + Sync {
    fn stored(&self, text: &str) -> Redacted;
}

/// Why a record isn't harvested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A run the hub made itself: its CLI completions, an agent's turn, or a
    /// process it started.
    HubInternal,
    /// The person turned memory off for this session.
    SessionOff,
    /// The person turned harvesting off for the session's group.
    GroupOff,
    /// Its folder is one `[desk.memory] exclude` names.
    Excluded,
    /// The person forgot it: a tombstone names its conversation or record.
    Forgotten,
    /// The person paused memory for every session.
    Paused,
}

impl Skip {
    /// What the person reads of why a session isn't remembered.
    pub fn plain(self) -> &'static str {
        match self {
            Skip::HubInternal => "This is the hub's own work, so Desk doesn't remember it.",
            Skip::SessionOff => "Remembering is off for this session.",
            Skip::GroupOff => "Remembering is off for this group.",
            Skip::Excluded => "This folder is one Desk is set never to remember.",
            Skip::Forgotten => "You asked Desk to forget this conversation.",
            Skip::Paused => "Remembering is paused for every session.",
        }
    }
}

/// A record whose harvest is being decided: its session, as the registry
/// has it, and the record itself.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub engine: Engine,
    pub cwd: &'a str,
    pub name: &'a str,
    /// The process of a session found in another terminal.
    pub pid: Option<i32>,
    pub cli_session_id: Option<&'a str>,
    /// The record's path.
    pub source: &'a str,
    /// The session's own switch (`sessions.memory`).
    pub memory_on: bool,
    /// The sidebar group it is in; none when ungrouped.
    pub group: Option<&'a str>,
}

/// The person's switches and the facts the harvest is checked against.
#[derive(Debug, Clone, Copy)]
pub struct Switches<'a> {
    /// `memory.paused`.
    pub paused: bool,
    /// Groups whose `memory.group.<id>` says `harvest = false`.
    pub groups_off: &'a HashSet<String>,
    /// `[desk.memory] exclude`, `~` expanded with `home`.
    pub exclude: &'a [String],
    pub home: Option<&'a str>,
    /// The refs tombstoned for the subject's engine: conversation ids and
    /// record paths.
    pub forgotten: &'a HashSet<String>,
    /// The process table, when it was read; empty otherwise.
    pub procs: &'a [crate::procs::Proc],
}

/// Whether `r` may be harvested, checked before anything of it is parsed;
/// the first reason it may not, if any.
pub fn harvestable(r: &Subject, hub: &HubInternal, s: &Switches) -> Result<(), Skip> {
    if hub.holds(Path::new(r.cwd))
        || crate::ledger::agent_turn(r.cwd, r.name)
        || r.pid.is_some_and(|pid| hub.started(s.procs, pid))
    {
        return Err(Skip::HubInternal);
    }
    if r.cli_session_id.is_some_and(|id| s.forgotten.contains(id)) || s.forgotten.contains(r.source)
    {
        return Err(Skip::Forgotten);
    }
    if !r.memory_on {
        return Err(Skip::SessionOff);
    }
    if r.group.is_some_and(|g| s.groups_off.contains(g)) {
        return Err(Skip::GroupOff);
    }
    if s.exclude
        .iter()
        .any(|pattern| excluded(pattern, r.cwd, s.home))
    {
        return Err(Skip::Excluded);
    }
    if s.paused {
        return Err(Skip::Paused);
    }
    Ok(())
}

/// Whether `cwd` is under a folder `pattern` names: `~/` is the home folder,
/// `*` any part of one name, and `**` any number of names. A folder is
/// excluded with everything inside it.
pub fn excluded(pattern: &str, cwd: &str, home: Option<&str>) -> bool {
    let pattern = pattern.trim();
    let expanded = match (pattern.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        (Some(_), None) => return false,
        (None, _) if pattern == "~" => match home {
            Some(home) => home.to_string(),
            None => return false,
        },
        (None, _) => pattern.to_string(),
    };
    let want: Vec<&str> = expanded.split('/').filter(|p| !p.is_empty()).collect();
    let have: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    !want.is_empty() && prefix_matches(&want, &have)
}

/// Whether the names `want` match a leading part of `have` (the rest of
/// `have` is inside it).
fn prefix_matches(want: &[&str], have: &[&str]) -> bool {
    match want.split_first() {
        None => true,
        Some((&"**", rest)) => (0..=have.len()).any(|skip| prefix_matches(rest, &have[skip..])),
        Some((name, rest)) => match have.split_first() {
            Some((got, more)) => name_matches(name, got) && prefix_matches(rest, more),
            None => false,
        },
    }
}

/// `*` in `pattern` matches any run of characters of one name.
fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len()
                && name.starts_with(head)
                && (0..=name.len() - head.len())
                    .filter(|i| name.is_char_boundary(head.len() + i))
                    .any(|i| name_matches(tail, &name[head.len() + i..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn subject<'a>(cwd: &'a str) -> Subject<'a> {
        Subject {
            engine: Engine::Claude,
            cwd,
            name: "acme-web",
            pid: None,
            cli_session_id: Some("5b1f0c2e"),
            source: "/Users/allan/.claude/projects/acme/5b1f0c2e.jsonl",
            memory_on: true,
            group: Some("acme"),
        }
    }

    #[test]
    fn every_reason_not_to_harvest_is_told_apart() {
        let hub =
            HubInternal::new([PathBuf::from("/data")], Some(Path::new("/nohome/k"))).started_by(50);
        let none = HashSet::new();
        let off: HashSet<String> = ["acme".to_string()].into();
        let tomb: HashSet<String> = ["5b1f0c2e".to_string()].into();
        let tomb_path: HashSet<String> =
            ["/Users/allan/.claude/projects/acme/5b1f0c2e.jsonl".to_string()].into();
        let exclude = ["~/clients/secret-x/**".to_string()];
        let procs = crate::procs::parse("  1 0 launchd\n 50 1 hub\n 51 50 claude -p\n");
        let open = Switches {
            paused: false,
            groups_off: &none,
            exclude: &exclude,
            home: Some("/nohome/k"),
            forgotten: &none,
            procs: &procs,
        };
        let check = |r: Subject, s: Switches| harvestable(&r, &hub, &s);

        assert_eq!(check(subject("/nohome/k/dev/acme-web"), open), Ok(()));
        assert_eq!(
            check(subject("/data/cli-completions/b369e00e"), open),
            Err(Skip::HubInternal),
            "a completion the hub ran in its own folder"
        );
        assert_eq!(
            check(
                subject("/elsewhere/agents/scout/projects/_agent-turn/work/s"),
                open
            ),
            Err(Skip::HubInternal),
            "an agent's turn"
        );
        assert_eq!(
            check(
                Subject {
                    pid: Some(51),
                    ..subject("/nohome/k/dev/acme-web")
                },
                open
            ),
            Err(Skip::HubInternal),
            "a process the hub started"
        );
        assert_eq!(
            check(
                subject("/nohome/k/dev/acme-web"),
                Switches {
                    forgotten: &tomb,
                    ..open
                }
            ),
            Err(Skip::Forgotten)
        );
        assert_eq!(
            check(
                subject("/nohome/k/dev/acme-web"),
                Switches {
                    forgotten: &tomb_path,
                    ..open
                }
            ),
            Err(Skip::Forgotten),
            "a tombstone on the record's path"
        );
        assert_eq!(
            check(
                Subject {
                    memory_on: false,
                    ..subject("/nohome/k/dev/acme-web")
                },
                open
            ),
            Err(Skip::SessionOff)
        );
        assert_eq!(
            check(
                subject("/nohome/k/dev/acme-web"),
                Switches {
                    groups_off: &off,
                    ..open
                }
            ),
            Err(Skip::GroupOff)
        );
        assert_eq!(
            check(
                Subject {
                    group: None,
                    ..subject("/nohome/k/dev/acme-web")
                },
                Switches {
                    groups_off: &off,
                    ..open
                }
            ),
            Ok(()),
            "an ungrouped session isn't the group's"
        );
        assert_eq!(
            check(subject("/nohome/k/clients/secret-x/api/src"), open),
            Err(Skip::Excluded)
        );
        assert_eq!(
            check(
                subject("/nohome/k/dev/acme-web"),
                Switches {
                    paused: true,
                    ..open
                }
            ),
            Err(Skip::Paused)
        );
    }

    #[test]
    fn exclusions_name_folders_and_everything_in_them() {
        let home = Some("/Users/allan");
        assert!(excluded(
            "~/clients/globex",
            "/Users/allan/clients/globex",
            home
        ));
        assert!(excluded(
            "~/clients/globex",
            "/Users/allan/clients/globex/api",
            home
        ));
        assert!(!excluded(
            "~/clients/globex",
            "/Users/allan/clients/globex-two",
            home
        ));
        assert!(excluded(
            "~/clients/*-x/**",
            "/Users/allan/clients/secret-x/a/b",
            home
        ));
        assert!(excluded("/srv/**/private", "/srv/a/b/private/c", None));
        assert!(!excluded("/srv/**/private", "/srv/a/b/public", None));
        assert!(!excluded("~/x", "/Users/allan/x", None), "no home, no ~");
        assert!(!excluded("", "/Users/allan", home));
    }
}
