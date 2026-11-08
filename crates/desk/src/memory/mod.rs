//! Session memory: what each session said and did, harvested from its
//! engine's own record by the ledger's reader, redacted before it is kept,
//! and never harvested from what is the hub's own or was forgotten. The
//! shapes and rules here are pure; the desk's I/O shell reads, stores and
//! searches.

pub mod derive;
pub mod harvest;
pub mod index;
pub mod passage;
pub mod rank;
pub mod reach;
pub mod record;
pub mod render;

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

use crate::adopt::HubInternal;
use crate::engine::Engine;

/// What session memory keeps, as the install's `[desk.memory]` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryOptions {
    /// Harvest sessions' conversations at all.
    pub enabled: bool,
    /// How far back a record not harvested before is read, by when its
    /// engine last wrote it.
    pub backfill_days: u32,
    /// How long a conversation is kept after it ended.
    pub retain_days: u32,
    /// Folders never harvested.
    pub exclude: Vec<String>,
    /// The most disk session memory may take, in MB: at 90 % the Floor
    /// says so, and at 100 % the harvest holds until there is room.
    pub max_disk_mb: u64,
    /// The most passages written to the search index a minute.
    pub index_per_minute: u32,
    /// The most a session's recall may cover within its group.
    pub session_reach: reach::Reach,
    /// Whether the hub's cheap model notes decisions, facts and to-dos from
    /// sessions (`derive = "cheap"`); off, nothing of a session is sent to
    /// it.
    pub derive: bool,
    /// The most calls it makes a day.
    pub derive_calls_per_day: u32,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            backfill_days: 30,
            retain_days: 180,
            exclude: Vec::new(),
            max_disk_mb: 4096,
            index_per_minute: 600,
            session_reach: reach::Reach::Project,
            derive: true,
            derive_calls_per_day: 200,
        }
    }
}

/// What the person reads when a session's engine keeps no record Desk can
/// read.
pub const NO_RECORD: &str =
    "Desk can't read this session's conversation, so it remembers only what it's for.";
/// What the person reads when the hub can't hide secrets.
pub const NO_REDACTOR: &str =
    "Desk can't hide secrets in conversations here, so it isn't remembering them.";
/// What the person reads when the install turned session memory off.
pub const DISABLED: &str = "Remembering conversations is turned off for this Desk.";
/// What the person reads of a session Desk no longer has.
pub const CLEARED: &str = "Desk no longer has this session; this is what it remembered of it.";
/// What the person reads while Desk Memory is out of room.
pub const FULL: &str = "Desk Memory is full, so Desk isn't remembering new conversations. Forget some sessions or raise max_disk_mb, and it carries on where it stopped.";
/// What the Floor says while deletions from the search index are set aside.
pub const PURGES_SET_ASIDE: &str = "Desk couldn't take some conversations you asked it to forget out of its search, though they're gone from everything else. Rebuild the search in Settings → Memory to clear them.";
/// What the Floor says as Desk Memory nears its limit.
pub const NEARLY_FULL: &str = "Desk Memory is nearly full. When it is, Desk stops remembering new conversations until you forget some sessions or raise max_disk_mb.";
/// What a search says while some of what it covers isn't in the index yet.
pub const CATCHING_UP: &str = "Desk is still adding some conversations to its search; until it has, those are found by keyword.";
/// What a keyword search says of itself until the search index is ready.
pub const KEYWORD_ONLY: &str = "Searching by keyword only until Desk Memory is ready.";

/// Text as it may be kept: every secret in it replaced whole, and how many
/// were. Text with none is the text itself, uncopied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted<'a> {
    pub text: Cow<'a, str>,
    pub count: u32,
}

/// Hides secrets in what is kept, with the hub's patterns; the desk can't
/// read them. Each secret is replaced in full by its label, with no part of
/// it kept.
pub trait Redactor: Send + Sync {
    fn stored<'a>(&self, text: &'a str) -> Redacted<'a>;

    /// What in `text`, already redacted, reads as instructions to a model:
    /// the hub's injection scan's categories. None without a scan.
    fn flags(&self, text: &str) -> Vec<String> {
        let _ = text;
        Vec::new()
    }
}

/// Why a record isn't harvested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A run the hub made itself: one in its folders (its CLI completions),
    /// or an agent's turn.
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
}

/// Whether `r` may be harvested, checked before anything of it is parsed;
/// the first reason it may not, if any.
pub fn harvestable(r: &Subject, hub: &HubInternal, s: &Switches) -> Result<(), Skip> {
    // A process the hub started is never adopted, so it has no record
    // here to check: its folder and its name are what tell.
    if hub.holds(Path::new(r.cwd)) || crate::ledger::agent_turn(r.cwd, r.name) {
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
            cli_session_id: Some("5b1f0c2e"),
            source: "/Users/allan/.claude/projects/acme/5b1f0c2e.jsonl",
            memory_on: true,
            group: Some("acme"),
        }
    }

    #[test]
    fn every_reason_not_to_harvest_is_told_apart() {
        let hub = HubInternal::new([PathBuf::from("/data")], Some(Path::new("/nohome/k")));
        let none = HashSet::new();
        let off: HashSet<String> = ["acme".to_string()].into();
        let tomb: HashSet<String> = ["5b1f0c2e".to_string()].into();
        let tomb_path: HashSet<String> =
            ["/Users/allan/.claude/projects/acme/5b1f0c2e.jsonl".to_string()].into();
        let exclude = ["~/clients/secret-x/**".to_string()];
        let open = Switches {
            paused: false,
            groups_off: &none,
            exclude: &exclude,
            home: Some("/nohome/k"),
            forgotten: &none,
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
