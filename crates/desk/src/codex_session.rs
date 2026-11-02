//! Which Codex session a `codex` typed in a Desk terminal is, from what
//! Codex keeps on disk: `~/.codex/session_index.jsonl` (one line per
//! session) and each session's rollout, whose first line is `session_meta`.
//! Only the fields wanted are read (ids, folders, event kinds); nothing of
//! a conversation's text is read here, held, or logged.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A session Codex has: its id and the folder it works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: String,
    pub cwd: String,
    pub rollout: PathBuf,
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

/// A rollout's session id and folder, from its first line; `None` for a
/// sub-agent's thread (its parent is the session) or a file that isn't one.
fn meta(rollout: &Path) -> Option<(String, String)> {
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
    Some((
        p["id"].as_str()?.to_string(),
        p["cwd"].as_str()?.to_string(),
    ))
}

fn bound(home: &Path, id: &str) -> Option<Binding> {
    let rollout = crate::codex::find_rollout(home, id)?;
    let (found, cwd) = meta(&rollout)?;
    (found == id).then_some(Binding {
        id: found,
        cwd,
        rollout,
    })
}

/// The session `codex resume <id>` opens, when Codex has it.
pub fn resumed(home: &Path, id: &str) -> Option<Binding> {
    bound(home, id)
}

/// The session a `codex` started with no id in `cwd` opened: the most
/// recently indexed one in that folder whose rollout was written since
/// `since`, and that `taken` doesn't already name.
pub fn started(
    home: &Path,
    cwd: &str,
    since: std::time::SystemTime,
    taken: impl Fn(&str) -> bool,
) -> Option<Binding> {
    indexed(home)
        .into_iter()
        .filter(|id| !taken(id))
        .filter_map(|id| bound(home, &id))
        .filter(|b| b.cwd == cwd)
        .find(|b| {
            std::fs::metadata(&b.rollout)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= since)
        })
}

/// Where the rollout's turns stand, from its tail.
pub fn activity(rollout: &Path) -> Activity {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 256 * 1024;
    let Ok(mut file) = std::fs::File::open(rollout) else {
        return Activity::Idle;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(TAIL);
    let mut raw = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.take(TAIL).read_to_end(&mut raw).is_err()
    {
        return Activity::Idle;
    }
    let text = String::from_utf8_lossy(&raw);
    text.lines()
        .skip(usize::from(start > 0))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "event_msg")
        .filter_map(|v| match v["payload"]["type"].as_str()? {
            "task_started" => Some(Activity::Working),
            "task_complete" | "turn_aborted" => Some(Activity::Done),
            _ => None,
        })
        .last()
        .unwrap_or(Activity::Idle)
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
        let dir = home.join(".codex/sessions/2026/11/02");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-11-02T10-00-00-{id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        let payload = serde_json::json!({"id": id, "cwd": cwd, "parent_thread_id": parent, "base_instructions": {"text": "x"}});
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

    #[test]
    fn a_resumed_session_is_its_id_and_folder() {
        let home = tempfile::tempdir().unwrap();
        session(home.path(), "a1", "/work/app", None, &[]);
        let b = resumed(home.path(), "a1").unwrap();
        assert_eq!((b.id.as_str(), b.cwd.as_str()), ("a1", "/work/app"));
        assert_eq!(resumed(home.path(), "nope"), None);
    }

    #[test]
    fn a_new_session_is_the_latest_one_in_its_folder_written_since_it_began() {
        let home = tempfile::tempdir().unwrap();
        session(home.path(), "old", "/work/app", None, &[]);
        session(home.path(), "other", "/work/else", None, &[]);
        session(home.path(), "mine", "/work/app", None, &[]);
        session(home.path(), "child", "/work/app", Some("mine"), &[]);
        index(
            home.path(),
            &[
                ("old", "2026-11-02T09:00:00Z"),
                ("other", "2026-11-02T10:05:00Z"),
                ("mine", "2026-11-02T10:01:00Z"),
                ("child", "2026-11-02T10:06:00Z"),
            ],
        );
        let long_ago = std::time::SystemTime::UNIX_EPOCH;
        let b = started(home.path(), "/work/app", long_ago, |_| false).unwrap();
        assert_eq!(b.id, "mine", "not another folder's, not a sub-agent's");
        let b = started(home.path(), "/work/app", long_ago, |id| id == "mine").unwrap();
        assert_eq!(b.id, "old", "one another terminal has is skipped");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(started(home.path(), "/work/app", later, |_| false), None);
    }

    #[test]
    fn a_turn_is_working_from_its_start_to_its_end() {
        let home = tempfile::tempdir().unwrap();
        let at = |tail: &[&str]| activity(&session(home.path(), "s", "/w", None, tail));
        assert_eq!(at(&[]), Activity::Idle);
        assert_eq!(at(&["task_started", "token_count"]), Activity::Working);
        assert_eq!(at(&["task_started", "task_complete"]), Activity::Done);
        assert_eq!(
            at(&["task_started", "task_complete", "task_started"]),
            Activity::Working
        );
        assert_eq!(at(&["task_started", "turn_aborted"]), Activity::Done);
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
