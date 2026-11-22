//! Desk's sight of its working sessions: one `ps` read a pass for all of
//! them, a `stat` of each record, the record's tail only when it grew, and
//! the newest lines of what the session prints. Each session keeps its last
//! [`Sample`], which the overview, the boss and the stall checks read
//! without any I/O of their own.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::Desk;
use crate::memory::Redactor;
use crate::model::{Engine, SessionRecord, SessionState, WaitsView};
use crate::procs::{self, Stat};
use crate::sight::procs::{self as sight_procs, Evidence};
use crate::sight::{self, OpenCall, Reading};

/// The most lines of output a sample keeps from each place it reads.
const OUTPUT_LINES: usize = 20;
/// How much of the screen is read for them.
const SCREEN_CHARS: usize = 4000;
/// How many of Claude Code's newest task output files are read.
const TASK_FILES: usize = 3;
/// How much of the end of a task output file is read.
const TASK_TAIL: u64 = 64 * 1024;
/// The most rows a process tree shows.
const TREE_ROWS: usize = 40;
/// The most of a process's arguments shown.
const ARGS_MAX: usize = 300;

/// How Desk samples what runs in its sessions.
#[derive(Debug, Clone)]
pub struct SightOptions {
    /// Where Claude Code keeps its sessions' task output
    /// (`/tmp/claude-<uid>`).
    pub task_root: PathBuf,
    /// How often the background pass samples.
    pub every: Duration,
    /// How old a sample may be before a read samples again.
    pub fresh: Duration,
    /// How far apart two samples are before a tree may read as still.
    pub settled: Duration,
    /// How long a sleep loop runs idle before it reads as a wait.
    pub poll: Duration,
    /// How long a wait lasts before the overview says it.
    pub shown_after: Duration,
}

impl Default for SightOptions {
    fn default() -> Self {
        // SAFETY: getuid only reads the calling process's user id.
        let uid = unsafe { libc::getuid() };
        Self {
            task_root: PathBuf::from(format!("/tmp/claude-{uid}")),
            every: Duration::from_secs(30),
            fresh: Duration::from_secs(90),
            settled: Duration::from_secs(10 * 60),
            poll: Duration::from_secs(10 * 60),
            shown_after: Duration::from_secs(2 * 60),
        }
    }
}

/// The sampler's own state: one pass at a time, and how many process
/// listings it has read.
#[derive(Default)]
pub(super) struct SightRun {
    pass: tokio::sync::Mutex<()>,
    reads: AtomicU64,
}

/// A record as last sampled: its path, length and last write.
type RecordSeen = (PathBuf, u64, SystemTime);

/// What Desk last saw of one session.
#[derive(Debug, Clone)]
pub struct Sample {
    pub at: DateTime<Utc>,
    /// When the first of its samples was taken.
    pub first_at: DateTime<Utc>,
    /// Whether this computer's processes could be listed.
    pub listed: bool,
    /// The process its work runs under.
    pub root: Option<u32>,
    /// Its work's processes, root first.
    pub tree: Vec<Stat>,
    runs: HashMap<u32, (u64, u64)>,
    /// Whether its work used the CPU since the sample before.
    pub cpu_growing: HashSet<u32>,
    pub active_at: DateTime<Utc>,
    records: Vec<RecordSeen>,
    /// When one of its records last grew.
    pub grew_at: DateTime<Utc>,
    /// How many records it has.
    pub record_count: usize,
    pub open_call: Option<OpenCall>,
    tasks: Vec<(PathBuf, SystemTime)>,
    task_lines: Vec<String>,
    /// Its newest output lines, redacted, oldest first.
    pub output: Vec<String>,
    output_hash: u64,
    /// When its output last changed.
    pub output_at: DateTime<Utc>,
    pub reading: Reading,
    /// The wait the overview says, once it has lasted
    /// [`SightOptions::shown_after`].
    pub waits: Option<WaitsView>,
}

/// The wait the overview says of `reading` at `at`, once it has lasted
/// `after`.
fn waits_of(reading: &Reading, at: DateTime<Utc>, after: Duration) -> Option<WaitsView> {
    let lasted = (at - reading.since).to_std().unwrap_or_default();
    (reading.kind.holds_up() && lasted >= after).then(|| WaitsView {
        words: reading.words.clone(),
        since: reading.since,
    })
}

/// A session's processes, as the boss reads them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "seen", rename_all = "snake_case")]
pub enum Processes {
    /// Nothing of it runs on this computer.
    NotRunning,
    /// This computer's processes can't be listed.
    Unlisted,
    Tree(ProcessTree),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessTree {
    pub root: u32,
    pub reading: Reading,
    pub rows: Vec<ProcessRow>,
    /// How many processes were left out.
    pub more: usize,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub depth: usize,
    pub program: String,
    /// Its arguments, redacted; none where secrets can't be hidden.
    pub args: Option<String>,
    pub state: String,
    pub ran: String,
    pub cpu: String,
    pub cpu_growing: bool,
    pub rss: String,
}

/// What the pass reads for one session, taken under the state's lock.
struct Due {
    id: Uuid,
    record: SessionRecord,
    root: u32,
    baseline: Option<HashSet<u32>>,
    prev: Option<Sample>,
}

/// What one session's files said this pass.
struct Files {
    records: Vec<RecordSeen>,
    open_call: Option<Option<OpenCall>>,
    tasks: Vec<(PathBuf, SystemTime)>,
    task_lines: Option<Vec<String>>,
}

fn hash_of(lines: &[String]) -> u64 {
    let mut h = DefaultHasher::new();
    lines.hash(&mut h);
    h.finish()
}

fn to_utc(t: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// Each line of `text` redacted, the last `OUTPUT_LINES` that aren't empty;
/// none without a redactor.
fn redacted_lines(text: &str, redactor: Option<&dyn Redactor>) -> Vec<String> {
    let Some(r) = redactor else {
        return Vec::new();
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(OUTPUT_LINES)..]
        .iter()
        .map(|l| r.stored(l).text.into_owned())
        .collect()
}

/// Claude Code's task output folder for a session whose transcript is
/// `transcript`: under `root`, its project folder's name, then its CLI id.
/// Only when it is a real folder inside `root`.
fn tasks_folder(root: &Path, transcript: &Path, cli: &str) -> Option<PathBuf> {
    let project = transcript.parent()?.file_name()?;
    let root = root.canonicalize().ok()?;
    let folder = root.join(project).join(cli).join("tasks");
    let real = folder.canonicalize().ok()?;
    let plain = std::fs::symlink_metadata(&folder)
        .ok()?
        .file_type()
        .is_dir();
    (plain && real.starts_with(&root)).then_some(real)
}

/// The newest task output files in `folder`, plain files only, newest
/// first.
fn newest_tasks(folder: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "output"))
        .filter_map(|e| {
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            meta.file_type()
                .is_file()
                .then(|| Some((e.path(), meta.modified().ok()?)))
                .flatten()
        })
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found.truncate(TASK_FILES);
    found
}

/// The open call at the end of `path`, an `engine` record.
fn open_call_in(path: &Path, engine: Engine) -> Option<OpenCall> {
    let text = crate::transcript::tail_lines(path).ok()?;
    let mut codex = crate::memory::harvest::CodexParts::default();
    let lines = text.lines().filter_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let at = crate::memory::harvest::time(&v, "/timestamp")?;
        let parts = match engine {
            Engine::Claude => crate::memory::harvest::claude_part(&v),
            Engine::Codex => crate::memory::harvest::codex_part(&v, &mut codex),
            Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => Vec::new(),
        };
        Some((at, parts))
    });
    sight::open_call(lines)
}

/// What a session's files say since `prev`: each record's length and last
/// write, the open call when one grew, and the task output when a file
/// changed.
fn read_files(
    records: &[PathBuf],
    engine: Engine,
    tasks: Option<PathBuf>,
    prev: Option<&Sample>,
    redactor: Option<&dyn Redactor>,
) -> Files {
    let seen: Vec<RecordSeen> = records
        .iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            Some((p.clone(), meta.len(), meta.modified().ok()?))
        })
        .collect();
    let grew = prev.is_none_or(|s| s.records != seen);
    let open_call = grew.then(|| {
        seen.iter()
            .filter_map(|(p, ..)| open_call_in(p, engine))
            .max_by_key(|c| c.since)
            .map(|c| OpenCall {
                input: redactor
                    .map(|r| {
                        crate::crew::clip(
                            &r.stored(&c.input).text,
                            crate::memory::record::INPUT_MAX,
                        )
                    })
                    .unwrap_or_default(),
                ..c
            })
    });
    let newest = tasks.as_deref().map(newest_tasks).unwrap_or_default();
    let changed = prev.is_none_or(|s| s.tasks != newest);
    let task_lines = changed.then(|| {
        newest
            .iter()
            .rev()
            .filter_map(|(p, _)| crate::transcript::tail_of(p, TASK_TAIL).ok())
            .flat_map(|t| redacted_lines(&t, redactor))
            .collect()
    });
    Files {
        records: seen,
        open_call,
        tasks: newest,
        task_lines,
    }
}

/// `secs` as `ps` says CPU time: "0:03", "2:05:03".
fn clock(secs: u64) -> String {
    match secs / 3600 {
        0 => format!("{}:{:02}", secs / 60, secs % 60),
        h => format!("{h}:{:02}:{:02}", secs / 60 % 60, secs % 60),
    }
}

/// A process tree's rows: the reading's own chain from the root first,
/// then the longest running, at most [`TREE_ROWS`].
fn rows_of(s: &Sample, redactor: Option<&dyn Redactor>) -> (Vec<ProcessRow>, usize) {
    let by_pid: HashMap<u32, &Stat> = s.tree.iter().map(|p| (p.proc.pid, p)).collect();
    let up = |pid: u32| {
        std::iter::successors(by_pid.get(&pid).copied(), |p| {
            by_pid.get(&p.proc.ppid).copied()
        })
        .take(64)
    };
    let mut chain: Vec<&Stat> = s
        .reading
        .pid
        .map(|pid| up(pid).collect())
        .unwrap_or_default();
    chain.reverse();
    let chained: HashSet<u32> = chain.iter().map(|p| p.proc.pid).collect();
    let mut rest: Vec<&Stat> = s
        .tree
        .iter()
        .filter(|p| !chained.contains(&p.proc.pid))
        .collect();
    rest.sort_by_key(|p| std::cmp::Reverse(p.elapsed_secs));
    let all: Vec<&Stat> = chain.into_iter().chain(rest).collect();
    let more = all.len().saturating_sub(TREE_ROWS);
    let rows = all
        .into_iter()
        .take(TREE_ROWS)
        .map(|p| ProcessRow {
            pid: p.proc.pid,
            ppid: p.proc.ppid,
            depth: up(p.proc.pid).count().saturating_sub(1),
            program: p.proc.program().unwrap_or_default(),
            args: redactor.map(|r| crate::crew::clip(&r.stored(&p.proc.args).text, ARGS_MAX)),
            state: format!("{}{}", p.state, if p.foreground { "+" } else { "" }),
            ran: sight::lasted(chrono::Duration::seconds(
                i64::try_from(p.elapsed_secs).unwrap_or(i64::MAX / 2),
            )),
            cpu: clock(p.cpu_secs),
            cpu_growing: s.cpu_growing.contains(&p.proc.pid),
            rss: format!("{} MB", p.rss_kb / 1024),
        })
        .collect();
    (rows, more)
}

impl Desk {
    /// The sampler's options.
    pub fn sight_options(&self) -> &SightOptions {
        &self.0.opts.sight
    }

    /// How many process listings the sampler has read.
    pub fn sight_reads(&self) -> u64 {
        self.0.sight.reads.load(Ordering::Relaxed)
    }

    /// What Desk last saw of `id`, when it sampled it.
    pub fn sample(&self, id: Uuid) -> Option<Sample> {
        self.0.state.lock().sight.get(&id).cloned()
    }

    /// What Desk saw of `id`, sampled again first when the last sample is
    /// older than [`SightOptions::fresh`].
    pub async fn sample_fresh(&self, id: Uuid) -> Option<Sample> {
        let stale = self.sample(id).is_none_or(|s| {
            (Utc::now() - s.at).to_std().unwrap_or_default() >= self.0.opts.sight.fresh
        });
        if stale {
            self.sight_pass().await;
        }
        self.sample(id)
    }

    /// The process a session's work runs under on this computer: its own
    /// process, an adopted one's, or the shell of the Desk terminal it was
    /// typed in. None when nothing of it runs here.
    pub fn process_root(&self, id: Uuid) -> Option<u32> {
        let st = self.0.state.lock();
        let r = st.records.get(&id)?;
        Self::root_in(&st, r)
    }

    fn root_in(st: &super::State, r: &SessionRecord) -> Option<u32> {
        match st.live.get(&r.id) {
            Some(l) => Self::root_of(l, r.engine).or_else(|| l.terminal.and(l.pty.pid())),
            None => r
                .external_pid
                .filter(|p| crate::adopt::alive(*p))
                .and_then(|p| u32::try_from(p).ok()),
        }
    }

    /// The sessions a pass samples: those working, or waiting on their own
    /// background work.
    fn sight_due(&self) -> Vec<Due> {
        let st = self.0.state.lock();
        st.records
            .values()
            .filter(|r| {
                let background = st
                    .live
                    .get(&r.id)
                    .is_some_and(|l| l.background.view().is_some());
                background
                    || matches!(
                        r.state,
                        SessionState::Starting | SessionState::Working | SessionState::Blocked
                    )
            })
            .filter_map(|r| {
                let root = Self::root_in(&st, r)?;
                Some(Due {
                    id: r.id,
                    record: r.clone(),
                    root,
                    baseline: st.live.get(&r.id).and_then(|l| l.baseline.clone()),
                    prev: st.sight.get(&r.id).cloned(),
                })
            })
            .collect()
    }

    /// Samples every session working or waiting on background work, with
    /// one process listing for all of them and for their background look.
    /// Reads nothing when there are none.
    pub async fn sight_pass(&self) {
        let _pass = self.0.sight.pass.lock().await;
        let due = self.sight_due();
        let live: Vec<Uuid> = self.0.state.lock().live.keys().copied().collect();
        let background = self.background_due(&live);
        if due.is_empty() && background.is_empty() {
            self.sight_keep(HashMap::new());
            return;
        }
        let table = procs::stats().await;
        self.0.sight.reads.fetch_add(1, Ordering::Relaxed);
        if let Some(table) = &table {
            self.background_looked(background, table);
        }
        let mut samples = HashMap::new();
        for d in due {
            let sample = self.sampled(d.prev.as_ref(), &d, table.as_deref()).await;
            samples.insert(d.id, sample);
        }
        self.sight_keep(samples);
    }

    /// Keeps `samples`, drops those of sessions no longer sampled, and
    /// tells every Desk watching of a wait the overview now says or no
    /// longer does.
    fn sight_keep(&self, samples: HashMap<Uuid, Sample>) {
        let changed: Vec<Uuid> = {
            let mut st = self.0.state.lock();
            let old = std::mem::replace(&mut st.sight, samples);
            let ids: HashSet<Uuid> = old.keys().chain(st.sight.keys()).copied().collect();
            ids.into_iter()
                .filter(|id| {
                    old.get(id).and_then(|s| s.waits.as_ref())
                        != st.sight.get(id).and_then(|s| s.waits.as_ref())
                })
                .collect()
        };
        changed.into_iter().for_each(|id| self.publish(id));
    }

    /// One session's sample now, from `table` and its files, after `prev`.
    async fn sampled(&self, prev: Option<&Sample>, d: &Due, table: Option<&[Stat]>) -> Sample {
        let now = Utc::now();
        let redactor = self.0.ledger.redactor.clone();
        let tree: Vec<Stat> = table
            .map(|all| {
                sight_procs::work(all, d.root, d.record.engine, d.baseline.as_ref())
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let files = {
            let desk = self.clone();
            let record = d.record.clone();
            let prev = prev.cloned();
            let task_root = self.0.opts.sight.task_root.clone();
            let redactor = redactor.clone();
            tokio::task::spawn_blocking(move || {
                let records = desk.records_of(&record);
                let tasks = match (record.engine, records.first(), &record.cli_session_id) {
                    (Engine::Claude, Some(t), Some(cli)) => tasks_folder(&task_root, t, cli),
                    _ => None,
                };
                read_files(
                    &records,
                    record.engine,
                    tasks,
                    prev.as_ref(),
                    redactor.as_deref(),
                )
            })
            .await
            .ok()
        };
        let files = files.unwrap_or(Files {
            records: prev.map(|p| p.records.clone()).unwrap_or_default(),
            open_call: None,
            tasks: prev.map(|p| p.tasks.clone()).unwrap_or_default(),
            task_lines: None,
        });
        let screen = self
            .terminal_text(d.id, SCREEN_CHARS)
            .map(|t| redacted_lines(&t, redactor.as_deref()))
            .unwrap_or_default();
        let task_lines = files
            .task_lines
            .unwrap_or_else(|| prev.map(|p| p.task_lines.clone()).unwrap_or_default());
        let output: Vec<String> = screen.into_iter().chain(task_lines.clone()).collect();
        let output_hash = hash_of(&output);
        let refs: Vec<&Stat> = tree.iter().collect();
        let growing: HashSet<u32> = prev
            .map(|p| {
                refs.iter()
                    .filter(|s| sight_procs::active(&p.runs, std::slice::from_ref(*s)))
                    .map(|s| s.proc.pid)
                    .collect()
            })
            .unwrap_or_default();
        let active_at = match prev {
            Some(p) if !sight_procs::active(&p.runs, &refs) => p.active_at,
            _ => now,
        };
        let last_write = files.records.iter().map(|(_, _, t)| to_utc(*t)).max();
        let grew_at = match prev {
            Some(p) if p.records == files.records => p.grew_at,
            Some(_) => now,
            None => last_write.unwrap_or(now),
        };
        let open_call = files
            .open_call
            .unwrap_or_else(|| prev.and_then(|p| p.open_call.clone()));
        let reading = sight_procs::classify(&Evidence {
            tree: &refs,
            open_call: open_call.as_ref(),
            output: &output,
            active_at,
            now,
            poll: chrono::Duration::from_std(self.0.opts.sight.poll).unwrap_or_default(),
        });
        let reading = match prev {
            Some(p) if p.reading.kind == reading.kind && p.reading.words == reading.words => {
                Reading {
                    since: p.reading.since.min(reading.since),
                    ..reading
                }
            }
            _ => reading,
        };
        Sample {
            at: now,
            first_at: prev.map_or(now, |p| p.first_at),
            listed: table.is_some(),
            root: Some(d.root),
            waits: waits_of(&reading, now, self.0.opts.sight.shown_after),
            runs: sight_procs::runs(&refs),
            cpu_growing: growing,
            active_at,
            record_count: files.records.len(),
            records: files.records,
            grew_at,
            open_call,
            tasks: files.tasks,
            task_lines,
            output_at: match prev {
                Some(p) if p.output_hash == output_hash => p.output_at,
                _ => now,
            },
            output,
            output_hash,
            reading,
            tree,
        }
    }

    /// What runs under `id` now, its reading first: sampled again when the
    /// last sample is stale.
    pub async fn process_tree(&self, id: Uuid) -> Processes {
        if self.process_root(id).is_none() {
            return Processes::NotRunning;
        }
        let Some(s) = self.sample_fresh(id).await else {
            return Processes::NotRunning;
        };
        let Some(root) = s.root.filter(|_| s.listed) else {
            return Processes::Unlisted;
        };
        let redactor = self.0.ledger.redactor.clone();
        let (rows, more) = rows_of(&s, redactor.as_deref());
        Processes::Tree(ProcessTree {
            root,
            reading: s.reading.clone(),
            rows,
            more,
            as_of: s.at,
        })
    }

    /// A session's sampled wait, for [`Self::view_of`].
    pub(super) fn waits_in(st: &super::State, id: Uuid) -> Option<WaitsView> {
        st.sight.get(&id).and_then(|s| s.waits.clone())
    }
}

#[cfg(test)]
pub(super) fn redactor_for_tests() -> Arc<dyn Redactor> {
    struct Hides;
    impl Redactor for Hides {
        fn stored<'a>(&self, text: &'a str) -> crate::memory::Redacted<'a> {
            match text.contains("SECRET-") {
                true => crate::memory::Redacted {
                    text: std::borrow::Cow::Owned(
                        text.split_whitespace()
                            .map(|w| match w.contains("SECRET-") {
                                true => "[secret]",
                                false => w,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    count: 1,
                },
                false => crate::memory::Redacted {
                    text: std::borrow::Cow::Borrowed(text),
                    count: 0,
                },
            }
        }
    }
    Arc::new(Hides)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_lines_are_redacted_and_none_are_kept_without_a_redactor() {
        let r = redactor_for_tests();
        let text = "one\n\n  \nkey SECRET-abc here\nthree";
        assert_eq!(
            redacted_lines(text, Some(r.as_ref())),
            ["one", "key [secret] here", "three"]
        );
        assert!(redacted_lines(text, None).is_empty());
        let many: String = (0..30).map(|n| format!("line {n}\n")).collect();
        let kept = redacted_lines(&many, Some(r.as_ref()));
        assert_eq!(kept.len(), OUTPUT_LINES);
        assert_eq!(kept.last().map(String::as_str), Some("line 29"));
    }

    #[test]
    fn cpu_time_is_said_as_ps_says_it() {
        assert_eq!(clock(3), "0:03");
        assert_eq!(clock(125), "2:05");
        assert_eq!(clock(7503), "2:05:03");
    }

    #[test]
    fn a_task_folder_is_only_a_real_one_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude-1");
        let tasks = root.join("-h-p").join("cli-1").join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let transcript = Path::new("/h/.claude/projects/-h-p/cli-1.jsonl");
        assert_eq!(
            tasks_folder(&root, transcript, "cli-1"),
            Some(tasks.canonicalize().unwrap())
        );
        assert_eq!(tasks_folder(&root, transcript, "cli-2"), None);
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(root.join("-h-q").join("cli-1")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("-h-q").join("cli-1").join("tasks"))
            .unwrap();
        let linked = Path::new("/h/.claude/projects/-h-q/cli-1.jsonl");
        assert_eq!(tasks_folder(&root, linked, "cli-1"), None);
    }

    #[test]
    fn the_newest_task_files_are_read_and_links_are_not() {
        let dir = tempfile::tempdir().unwrap();
        for (n, name) in ["a", "b", "c", "d"].iter().enumerate() {
            let p = dir.path().join(format!("{name}.output"));
            std::fs::write(&p, format!("{name}\n")).unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + n as u64);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("a.output"), dir.path().join("z.output"))
            .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let names: Vec<String> = newest_tasks(dir.path())
            .iter()
            .map(|(p, _)| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["d", "c", "b"]);
    }

    #[test]
    fn a_trees_rows_put_its_readings_chain_first() {
        let stat = |pid, ppid, args: &str, ran| Stat {
            proc: procs::Proc {
                pid,
                ppid,
                args: args.into(),
            },
            state: 'S',
            foreground: false,
            elapsed_secs: ran,
            cpu_secs: 3,
            rss_kb: 217_088,
        };
        let now = Utc::now();
        let tree = vec![
            stat(1, 0, "claude", 9000),
            stat(2, 1, "zsh -c x", 50),
            stat(3, 2, "cargo test SECRET-abc", 40),
            stat(4, 1, "zsh -c y", 8000),
        ];
        let s = Sample {
            at: now,
            first_at: now,
            listed: true,
            root: Some(1),
            runs: HashMap::new(),
            cpu_growing: HashSet::from([3]),
            active_at: now,
            records: vec![],
            grew_at: now,
            record_count: 0,
            open_call: None,
            tasks: vec![],
            task_lines: vec![],
            output: vec![],
            output_hash: 0,
            output_at: now,
            reading: Reading {
                pid: Some(3),
                ..Reading::quiet(now)
            },
            waits: None,
            tree,
        };
        let r = redactor_for_tests();
        let (rows, more) = rows_of(&s, Some(r.as_ref()));
        assert_eq!(more, 0);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[2].args.as_deref(), Some("cargo test [secret]"));
        assert!(rows[2].cpu_growing);
        assert_eq!(rows[2].rss, "212 MB");
        assert_eq!(rows[0].ran, "2 h 30 min");
        let (bare, _) = rows_of(&s, None);
        assert!(bare.iter().all(|r| r.args.is_none()));
    }
}
