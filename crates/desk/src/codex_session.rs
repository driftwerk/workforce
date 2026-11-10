//! Which Codex session a `codex` typed in a Desk terminal is, from what
//! Codex keeps on disk: `~/.codex/session_index.jsonl` (one line per
//! session) and each session's rollout, whose first line is `session_meta`.
//! Only the fields wanted are read (ids, folders, event kinds); nothing of
//! a conversation's text is read here, held, or logged.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

/// A session Codex has: its id, the folder it works in, and when it was
/// created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: String,
    pub cwd: String,
    pub rollout: PathBuf,
    pub created: Option<DateTime<Utc>>,
}

/// Codex's rollout files, listed once for a round of lookups.
pub struct Rollouts(Vec<PathBuf>);

impl Rollouts {
    pub fn of(home: &Path) -> Self {
        Self(crate::codex::rollouts(home))
    }

    fn find(&self, id: &str) -> Option<&PathBuf> {
        let suffix = format!("-{id}.jsonl");
        self.0.iter().find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
    }
}

/// Where a session's turn stands, as its rollout last said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// No turn yet.
    Idle,
    /// A turn began and has not ended.
    Working,
    /// The last turn ended.
    Done,
}

/// The ids in the session index, most recently updated first.
fn indexed(home: &Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(home.join(".codex/session_index.jsonl")) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, String)> = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let v: Value = serde_json::from_str(&l).ok()?;
            Some((
                v["updated_at"].as_str().unwrap_or_default().to_string(),
                v["id"].as_str()?.to_string(),
            ))
        })
        .collect();
    rows.sort_by(|a, b| b.cmp(a));
    rows.into_iter().map(|(_, id)| id).collect()
}

/// A rollout's session id, folder and creation time, from its first line;
/// `None` for a sub-agent's thread (its parent is the session) or a file
/// that isn't a rollout.
fn meta(rollout: &Path) -> Option<(String, String, Option<DateTime<Utc>>)> {
    let mut first = String::new();
    BufReader::new(std::fs::File::open(rollout).ok()?)
        .read_line(&mut first)
        .ok()?;
    let v: Value = serde_json::from_str(&first).ok()?;
    if v["type"] != "session_meta" {
        return None;
    }
    let p = &v["payload"];
    if p["parent_thread_id"].as_str().is_some() {
        return None;
    }
    let created = p["timestamp"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    Some((
        p["id"].as_str()?.to_string(),
        p["cwd"].as_str()?.to_string(),
        created,
    ))
}

fn bound(rollouts: &Rollouts, id: &str) -> Option<Binding> {
    let rollout = rollouts.find(id)?.clone();
    let (found, cwd, created) = meta(&rollout)?;
    (found == id).then_some(Binding {
        id: found,
        cwd,
        rollout,
        created,
    })
}

/// The session `codex resume <id>` opens, when Codex has it.
pub fn resumed(rollouts: &Rollouts, id: &str) -> Option<Binding> {
    bound(rollouts, id)
}

/// The session a `codex` started with no id in `cwd` opened: the first one
/// in that folder *created* since the process began (a session another
/// tool is working in was created before, however recently it was
/// written), that `taken` doesn't already name. `None` when there is none
/// yet, or its creation time can't be read.
pub fn started(
    home: &Path,
    rollouts: &Rollouts,
    cwd: &str,
    since: DateTime<Utc>,
    taken: impl Fn(&str) -> bool,
) -> Option<Binding> {
    indexed(home)
        .into_iter()
        .filter(|id| !taken(id))
        .filter_map(|id| bound(rollouts, &id))
        .filter(|b| b.cwd == cwd)
        .filter(|b| b.created.is_some_and(|c| c >= since))
        .min_by_key(|b| b.created)
}

/// What was last learned of a rollout's turns: how far it was read (up to
/// the end of its last whole line) and the state its latest turn marker
/// said, so a later look reads only the bytes added since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    rollout: PathBuf,
    read: u64,
    pub activity: Activity,
}

/// The state a line says, when it is a turn marker: `task_started`,
/// `task_complete` (what Codex writes, 0.166 included) or their newer names
/// `turn_started` and `turn_complete` (which its reader takes for them),
/// and `turn_aborted`. Only the event kind is read.
fn marker(line: &str) -> Option<Activity> {
    // Cheap test first: most lines are tool calls and output.
    if !line.contains("\"event_msg\"") || !line.contains("\"task_") && !line.contains("\"turn_") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"] != "event_msg" {
        return None;
    }
    match v["payload"]["type"].as_str()? {
        "task_started" | "turn_started" => Some(Activity::Working),
        "task_complete" | "turn_complete" | "turn_aborted" => Some(Activity::Done),
        _ => None,
    }
}

/// The latest marker among the whole lines of `bytes`, and how many bytes
/// those lines take (a half-written last line is left for next time).
fn scan(bytes: &[u8]) -> (Option<Activity>, usize) {
    let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let latest = String::from_utf8_lossy(&bytes[..whole])
        .lines()
        .filter_map(marker)
        .next_back();
    (latest, whole)
}

fn read_range(file: &mut std::fs::File, from: u64, to: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut raw = Vec::new();
    file.take(to - from).read_to_end(&mut raw).ok()?;
    Some(raw)
}

/// Where the rollout's turns stand: its latest turn marker, wherever in the
/// file it is. Searched back from the end in growing chunks (a turn of
/// hundreds of tool calls starts far from the end), up to `FAR`, then the
/// whole file. With `prev` (from an earlier look), only the bytes added
/// since are read; no new marker leaves the state as it was, and a file
/// that got shorter is read afresh. `None` when it can't be read.
pub fn activity(rollout: &Path, prev: Option<&Mark>) -> Option<Mark> {
    const FIRST: u64 = 256 * 1024;
    const FAR: u64 = 32 * 1024 * 1024;
    let mut file = std::fs::File::open(rollout).ok()?;
    let len = file.metadata().ok()?.len();
    let mark = |read, activity| Mark {
        rollout: rollout.to_path_buf(),
        read,
        activity,
    };
    if let Some(p) = prev.filter(|p| p.rollout == rollout && p.read <= len) {
        let bytes = read_range(&mut file, p.read, len)?;
        let (latest, whole) = scan(&bytes);
        return Some(mark(p.read + whole as u64, latest.unwrap_or(p.activity)));
    }
    let mut window = FIRST;
    loop {
        let start = len.saturating_sub(window);
        let mut bytes = read_range(&mut file, start, len)?;
        let mut skipped = 0;
        if start > 0 {
            // Begins mid-line, probably: from the first whole line.
            skipped = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |i| i + 1);
            bytes.drain(..skipped);
        }
        let (latest, whole) = scan(&bytes);
        let read = start + (skipped + whole) as u64;
        if let Some(a) = latest {
            return Some(mark(read, a));
        }
        if start == 0 {
            return Some(mark(read, Activity::Idle));
        }
        window = if window >= FAR { len } else { window * 2 };
    }
}

/// Whether a `codex` command line is the interactive TUI: not `exec`,
/// `review`, `mcp-server`, `login`, `app-server` or another subcommand
/// that runs without a conversation of the person's.
pub fn is_tui(args: &str) -> bool {
    const OTHER: [&str; 16] = [
        "exec",
        "e",
        "review",
        "mcp-server",
        "mcp",
        "login",
        "logout",
        "app-server",
        "proto",
        "completion",
        "apply",
        "a",
        "sandbox",
        "debug",
        "features",
        "plugin",
    ];
    let first = args
        .split_whitespace()
        .skip(1)
        .find(|w| !w.starts_with('-'));
    !first.is_some_and(|w| OTHER.contains(&w))
}

/// The session id a `codex` command line resumes, if it says one.
pub fn resume_arg(args: &str) -> Option<&str> {
    let mut words = args.split_whitespace();
    words.by_ref().find(|w| *w == "resume")?;
    words
        .find(|w| !w.starts_with('-'))
        .filter(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Seconds a process has run, from `ps -o etime=` (`[[dd-]hh:]mm:ss`).
pub fn parse_etime(etime: &str) -> Option<u64> {
    let (days, rest) = match etime.trim().split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, etime.trim()),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let secs = match parts[..] {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(days * 86_400 + secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn session(home: &Path, id: &str, cwd: &str, parent: Option<&str>, tail: &[&str]) -> PathBuf {
        session_at(home, id, cwd, parent, "2026-11-02T10:00:00Z", tail)
    }

    fn session_at(
        home: &Path,
        id: &str,
        cwd: &str,
        parent: Option<&str>,
        created: &str,
        tail: &[&str],
    ) -> PathBuf {
        let dir = home.join(".codex/sessions/2026/11/02");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-11-02T10-00-00-{id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        let payload = serde_json::json!({"id": id, "cwd": cwd, "parent_thread_id": parent, "timestamp": created, "base_instructions": {"text": "x"}});
        writeln!(
            f,
            "{}",
            serde_json::json!({"type": "session_meta", "payload": payload})
        )
        .unwrap();
        for kind in tail {
            writeln!(
                f,
                "{}",
                serde_json::json!({"type": "event_msg", "payload": {"type": kind}})
            )
            .unwrap();
        }
        path
    }

    fn index(home: &Path, rows: &[(&str, &str)]) {
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let mut f = std::fs::File::create(home.join(".codex/session_index.jsonl")).unwrap();
        for (id, at) in rows {
            writeln!(
                f,
                "{}",
                serde_json::json!({"id": id, "thread_name": "n", "updated_at": at})
            )
            .unwrap();
        }
    }

    fn time(t: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_resumed_session_is_its_id_and_folder() {
        let home = tempfile::tempdir().unwrap();
        session(home.path(), "a1", "/work/app", None, &[]);
        let all = Rollouts::of(home.path());
        let b = resumed(&all, "a1").unwrap();
        assert_eq!((b.id.as_str(), b.cwd.as_str()), ("a1", "/work/app"));
        assert_eq!(resumed(&all, "nope"), None);
    }

    #[test]
    fn a_new_session_is_the_first_one_created_in_its_folder_since_it_began() {
        let home = tempfile::tempdir().unwrap();
        session_at(
            home.path(),
            "busy",
            "/work/app",
            None,
            "2026-11-02T09:00:00Z",
            &["task_started"],
        );
        session_at(
            home.path(),
            "other",
            "/work/else",
            None,
            "2026-11-02T10:01:00Z",
            &[],
        );
        session_at(
            home.path(),
            "mine",
            "/work/app",
            None,
            "2026-11-02T10:01:00Z",
            &[],
        );
        session_at(
            home.path(),
            "later",
            "/work/app",
            None,
            "2026-11-02T10:30:00Z",
            &[],
        );
        session_at(
            home.path(),
            "child",
            "/work/app",
            Some("mine"),
            "2026-11-02T10:01:30Z",
            &[],
        );
        index(
            home.path(),
            &[
                // Another tool's session, active right now in the same folder.
                ("busy", "2026-11-02T10:02:00Z"),
                ("other", "2026-11-02T10:05:00Z"),
                ("mine", "2026-11-02T10:01:00Z"),
                ("later", "2026-11-02T10:30:00Z"),
                ("child", "2026-11-02T10:06:00Z"),
            ],
        );
        let all = Rollouts::of(home.path());
        let began = time("2026-11-02T10:00:50Z");
        let b = started(home.path(), &all, "/work/app", began, |_| false).unwrap();
        assert_eq!(
            b.id, "mine",
            "not the busy one, another folder's, or a sub-agent's"
        );
        let b = started(home.path(), &all, "/work/app", began, |id| id == "mine").unwrap();
        assert_eq!(b.id, "later", "one another terminal has is skipped");
        let none = time("2026-11-02T11:00:00Z");
        assert_eq!(
            started(home.path(), &all, "/work/app", none, |_| false),
            None
        );
    }

    #[test]
    fn a_turn_is_working_from_its_start_to_its_end() {
        let home = tempfile::tempdir().unwrap();
        let at = |tail: &[&str]| {
            activity(&session(home.path(), "s", "/w", None, tail), None)
                .unwrap()
                .activity
        };
        assert_eq!(at(&[]), Activity::Idle);
        assert_eq!(at(&["task_started", "token_count"]), Activity::Working);
        assert_eq!(at(&["task_started", "task_complete"]), Activity::Done);
        assert_eq!(
            at(&["task_started", "task_complete", "task_started"]),
            Activity::Working
        );
        assert_eq!(at(&["task_started", "turn_aborted"]), Activity::Done);
    }

    /// A rollout Codex 0.166 wrote (against a stand-in model), its session
    /// line first, kept as `id` in `home`.
    fn recorded(home: &Path, id: &str, recording: &str) -> PathBuf {
        let dir = home.join(".codex/sessions/2026/11/10");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-11-10T16-47-47-{id}.jsonl"));
        std::fs::write(&path, recording).unwrap();
        path
    }

    #[test]
    fn a_turn_ends_in_every_shape_codex_writes() {
        let home = tempfile::tempdir().unwrap();
        // Today's (0.166): `task_started` … `task_complete`, with its turn,
        // attribution, and a failed turn's error; or `turn_aborted`.
        for (id, recording) in [
            (
                "ok",
                include_str!("../fixtures/codex/rollout-0.166-ok.jsonl"),
            ),
            (
                "failed",
                include_str!("../fixtures/codex/rollout-0.166-failed.jsonl"),
            ),
            (
                "interrupted",
                include_str!("../fixtures/codex/rollout-0.166-interrupted.jsonl"),
            ),
        ] {
            // Up to its last line, working; with it, done.
            let (begun, _end) = recording.trim_end().rsplit_once('\n').unwrap();
            let path = recorded(home.path(), id, &format!("{begun}\n"));
            assert_eq!(
                activity(&path, None).unwrap().activity,
                Activity::Working,
                "{id}"
            );
            std::fs::write(&path, recording).unwrap();
            assert_eq!(
                activity(&path, None).unwrap().activity,
                Activity::Done,
                "{id}"
            );
        }
        // Older rollouts: the bare event kinds.
        let old = session(
            home.path(),
            "old",
            "/w",
            None,
            &["task_started", "task_complete"],
        );
        assert_eq!(activity(&old, None).unwrap().activity, Activity::Done);
        // The newer names Codex's reader takes for the same events.
        let new = session(home.path(), "new", "/w", None, &["turn_started"]);
        let m = activity(&new, None).unwrap();
        assert_eq!(m.activity, Activity::Working);
        append(&new, &["turn_complete"], 0);
        assert_eq!(activity(&new, Some(&m)).unwrap().activity, Activity::Done);
    }

    fn append(path: &Path, kinds: &[&str], filler: usize) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for kind in kinds {
            let line = serde_json::json!({"type": "event_msg", "payload": {"type": kind}});
            writeln!(f, "{line}").unwrap();
        }
        for _ in 0..filler {
            let line = serde_json::json!({"type": "response_item", "payload": {"type": "function_call", "pad": "x".repeat(1000)}});
            writeln!(f, "{line}").unwrap();
        }
    }

    #[test]
    fn a_turn_that_began_far_back_is_still_working() {
        let home = tempfile::tempdir().unwrap();
        let path = session(
            home.path(),
            "s",
            "/w",
            None,
            &["task_complete", "task_started"],
        );
        // Well over the first 256 KB window, and over the second.
        append(&path, &[], 1500);
        assert!(std::fs::metadata(&path).unwrap().len() > 1024 * 1024);
        let m = activity(&path, None).unwrap();
        assert_eq!(m.activity, Activity::Working);
        // Never a marker anywhere: idle, however long.
        let none = session(home.path(), "t", "/w", None, &[]);
        append(&none, &[], 600);
        assert_eq!(activity(&none, None).unwrap().activity, Activity::Idle);
    }

    #[test]
    fn later_bytes_are_all_that_is_read_and_a_marker_among_them_wins() {
        let home = tempfile::tempdir().unwrap();
        let path = session(home.path(), "s", "/w", None, &["task_started"]);
        append(&path, &[], 400);
        let first = activity(&path, None).unwrap();
        assert_eq!(first.activity, Activity::Working);
        // Nothing new, or no new marker: as it was.
        assert_eq!(activity(&path, Some(&first)).unwrap(), first);
        append(&path, &[], 10);
        let more = activity(&path, Some(&first)).unwrap();
        assert_eq!(more.activity, Activity::Working);
        assert!(more.read > first.read);
        append(&path, &["task_complete"], 3);
        let done = activity(&path, Some(&more)).unwrap();
        assert_eq!(done.activity, Activity::Done);
        append(&path, &["task_started"], 0);
        assert_eq!(
            activity(&path, Some(&done)).unwrap().activity,
            Activity::Working
        );
    }

    #[test]
    fn a_half_written_line_waits_for_its_end() {
        let home = tempfile::tempdir().unwrap();
        let path = session(home.path(), "s", "/w", None, &["task_started"]);
        let m = activity(&path, None).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(
            f,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_comp"
        )
        .unwrap();
        let m = activity(&path, Some(&m)).unwrap();
        assert_eq!(m.activity, Activity::Working);
        writeln!(f, "lete\"}}}}").unwrap();
        assert_eq!(activity(&path, Some(&m)).unwrap().activity, Activity::Done);
    }

    #[test]
    fn a_file_that_shrank_or_was_replaced_is_read_again() {
        let home = tempfile::tempdir().unwrap();
        let path = session(home.path(), "s", "/w", None, &["task_started"]);
        append(&path, &[], 100);
        let m = activity(&path, None).unwrap();
        assert_eq!(m.activity, Activity::Working);
        // Replaced by a shorter one that says the turn ended.
        let path = session(
            home.path(),
            "s",
            "/w",
            None,
            &["task_started", "task_complete"],
        );
        assert!(std::fs::metadata(&path).unwrap().len() < m.read);
        assert_eq!(activity(&path, Some(&m)).unwrap().activity, Activity::Done);
        // A mark of another file is not used.
        let other = session(home.path(), "u", "/w", None, &["task_started"]);
        assert_eq!(
            activity(&other, Some(&activity(&path, None).unwrap()))
                .unwrap()
                .activity,
            Activity::Working
        );
        assert_eq!(activity(&home.path().join("gone"), None), None);
    }

    #[test]
    fn only_the_interactive_codex_is_a_session() {
        assert!(is_tui("codex"));
        assert!(is_tui("codex resume abc --approve-for-me"));
        assert!(is_tui("/x/codex -m gpt"));
        for other in [
            "codex exec fix it",
            "codex review",
            "codex mcp-server",
            "codex login",
            "codex app-server --listen unix:///s",
            "codex --yolo exec x",
        ] {
            assert!(!is_tui(other), "{other}");
        }
    }

    #[test]
    fn a_resume_names_its_session_on_the_command_line() {
        assert_eq!(
            resume_arg("codex resume 0199-ab --approve-for-me"),
            Some("0199-ab")
        );
        assert_eq!(resume_arg("/x/codex resume --last"), None);
        assert_eq!(resume_arg("codex --remote unix:///s.sock"), None);
        assert_eq!(resume_arg("codex resume ../../etc"), None);
    }

    #[test]
    fn elapsed_times_are_read_in_every_form_ps_prints() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("12:07"), Some(727));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-01:02:03"), Some(2 * 86_400 + 3723));
        assert_eq!(parse_etime("soon"), None);
    }
}
