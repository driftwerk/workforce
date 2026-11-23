//! Desk's sight of its working sessions: one `ps` read a pass for all of
//! them, a `stat` of each record, the record's tail only when it grew, and
//! what its open call and background shells print, read on from where the
//! last pass left off. Each session keeps its last [`Sample`], which the
//! overview, the boss and the stall checks read without any I/O of their
//! own.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use uuid::Uuid;

use super::memory::Why;
use super::Desk;
use crate::memory::Redactor;
use crate::model::{Engine, SessionRecord, SessionState, WaitsView};
use crate::procs::{self, Stat};
use crate::sight::logs::{self, Logs};
use crate::sight::procs::{self as sight_procs, Evidence, Said};
use crate::sight::transcript::{self, Budget, Call, Hit, Ranges};
use crate::sight::{self, Glance, Growth, Kind, OpenCall, Reading};

/// The most lines of output a sample keeps from each place it reads.
const OUTPUT_LINES: usize = 20;
/// How much of the screen is read for them.
const SCREEN_CHARS: usize = 4000;
/// How many of the newest task output files made since the folder was first
/// listed are kept as the ones an open call may write to.
const BORN_KEPT: usize = 64;
/// The most of a task output file read in one pass.
const TASK_TAIL: u64 = 64 * 1024;
/// The tool whose calls write their output to a task output file.
const SHELL_TOOL: &str = "Bash";
/// The most rows a process tree shows.
const TREE_ROWS: usize = 40;
/// The most of a process's arguments shown.
const ARGS_MAX: usize = 300;
/// How long one search or step lookup may read for.
const SCAN_FOR: Duration = Duration::from_secs(3);
/// How far before a session's turn its logs are read from, and the most of
/// them read.
const LOGS_BEFORE: chrono::Duration = chrono::Duration::minutes(10);
const LOGS_MOST: chrono::Duration = chrono::Duration::hours(6);

/// Why a session's transcript isn't shown.
pub const AGENT_TURN: &str = "That's an agent's own turn: read it with crew.history (agent_id).";
pub const KEPT_OUT: &str = "The person keeps this session's conversation out of Desk.";
pub const NO_REDACTOR: &str =
    "Desk can't hide secrets on this computer, so it shows no transcript.";
pub const REPLACED: &str =
    "The transcript was replaced since that page: start again without before.";
pub const STEP_UNSEEN: &str = "That call isn't in the newest 32 MB of its transcript.";
pub const NO_SUCH_RECORD: &str = "It has no record by that name: use one of its records.";
const UNREAD: &str = "Its transcript couldn't be read.";
const UNCHECKED: &str = "Desk couldn't check whether this session's conversation may be read.";
/// Why a session's logs aren't shown.
pub const NO_LOGS: &str = "This hub keeps no Desk logs.";
pub const LOGS_NO_REDACTOR: &str = "Desk can't hide secrets on this computer, so it shows no logs.";

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
    /// Desk's own logs folder, where this hub is Desk's.
    pub logs: Option<PathBuf>,
    /// How long looks in a row must miss a background shell the CLI
    /// reported running before it is taken to have ended.
    pub shell_unseen: Duration,
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
            logs: None,
            shell_unseen: Duration::from_secs(2 * 60),
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
    pub root: u32,
    /// Its work's processes, root first.
    pub tree: Vec<Stat>,
    runs: HashMap<u32, (u64, u64)>,
    /// Which of its processes used the CPU since the sample before.
    pub cpu_growing: HashSet<u32>,
    /// When what runs under its root last used the CPU.
    pub active_at: DateTime<Utc>,
    records: Vec<RecordSeen>,
    /// When one of its records last grew.
    pub grew_at: DateTime<Utc>,
    /// How many records it has.
    pub record_count: usize,
    /// Its open call; its input left out where the person keeps the
    /// session out of Desk.
    pub open_call: Option<OpenCall>,
    folder: Option<Arc<TaskFolder>>,
    call_output: Option<Output>,
    shells: Vec<Output>,
    /// Its background shells found running in a process of their own
    /// ([`sight_procs::placed`]).
    pub found: BTreeSet<String>,
    /// What its open call's and background shells' own output says each
    /// waits on.
    said: Vec<Said>,
    /// Its newest lines on screen, then from its open call's and its
    /// background shells' output, redacted, oldest first; none where the
    /// person keeps the session out of Desk.
    pub output: Vec<String>,
    /// When its open call's or background shells' output last grew.
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
    /// The ids of its background shells.
    shells: Vec<String>,
    prev: Option<Sample>,
}

/// A task output file, by the id of the call or background shell that
/// writes it.
type Output = (String, Tracked);

/// A task output file as last read.
#[derive(Debug, Clone, PartialEq)]
struct Tracked {
    path: PathBuf,
    /// When it was born, where its system keeps that: when the command
    /// writing it started.
    born: Option<DateTime<Utc>>,
    /// Its length then.
    len: u64,
    /// How far its whole lines were read.
    read_to: u64,
    /// Its newest lines, redacted, oldest first.
    lines: Vec<String>,
    /// Whether its last line says its command ended.
    ended: bool,
}

impl Tracked {
    fn from(path: PathBuf, born: Option<SystemTime>, read_to: u64) -> Self {
        Self {
            path,
            born: born.map(to_utc),
            len: read_to,
            read_to,
            lines: Vec::new(),
            ended: false,
        }
    }
}

/// Whether `line` is what Claude Code writes last in a task output file
/// once its command ended: "[exited with code 0]", "[killed]".
fn says_ended(line: &str) -> bool {
    static ENDED: OnceLock<Regex> = OnceLock::new();
    ENDED
        .get_or_init(|| {
            Regex::new(r"^\[(exited with code -?\d+|killed)\]$").expect("a valid pattern")
        })
        .is_match(line.trim())
}

/// A session's task output folder as last listed: when it was first
/// listed, its last write, every name in it, and the newest output files in
/// it, with when each was made.
#[derive(Debug)]
struct TaskFolder {
    path: PathBuf,
    first: SystemTime,
    changed: SystemTime,
    names: HashSet<OsString>,
    born: Vec<(PathBuf, SystemTime)>,
}

/// What one session's files said this pass.
struct Files {
    records: Vec<RecordSeen>,
    open_call: Option<OpenCall>,
    folder: Option<Arc<TaskFolder>>,
    call_output: Option<Output>,
    shells: Vec<Output>,
    /// The lines that appeared in each output this pass, by its id.
    new: Vec<(String, Vec<String>)>,
}

impl Files {
    /// What `prev` read, read again with nothing new.
    fn kept(prev: Option<&Sample>) -> Self {
        Self {
            records: prev.map(|p| p.records.clone()).unwrap_or_default(),
            open_call: prev.and_then(|p| p.open_call.clone()),
            folder: prev.and_then(|p| p.folder.clone()),
            call_output: prev.and_then(|p| p.call_output.clone()),
            shells: prev.map(|p| p.shells.clone()).unwrap_or_default(),
            new: Vec::new(),
        }
    }
}

/// Each task output file read and its length.
fn lengths<'a>(call: Option<&'a Output>, shells: &'a [Output]) -> Vec<(&'a Path, u64)> {
    call.into_iter()
        .chain(shells)
        .map(|(_, t)| (t.path.as_path(), t.len))
        .collect()
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

/// `path`'s own metadata when it is a plain file, never a link.
fn plain(path: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_file())
}

/// When the file `meta` describes was made, where its system says.
fn made(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.created().or_else(|_| meta.modified()).ok()
}

/// `folder` listed again only when it changed since `prev`; only the names
/// it did not hold before are looked at.
fn listed(folder: &Path, prev: Option<&Arc<TaskFolder>>) -> Option<Arc<TaskFolder>> {
    let changed = std::fs::metadata(folder).ok()?.modified().ok()?;
    let prev = prev.filter(|f| f.path == folder);
    if let Some(same) = prev.filter(|f| f.changed == changed) {
        return Some(same.clone());
    }
    let names: HashSet<OsString> = std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.file_name())
        .collect();
    let new: Vec<(PathBuf, SystemTime)> = names
        .iter()
        .filter(|n| prev.is_none_or(|f| !f.names.contains(*n)))
        .map(|n| folder.join(n))
        .filter(|p| p.extension().is_some_and(|x| x == "output"))
        .filter_map(|p| {
            let made = made(&plain(&p)?)?;
            Some((p, made))
        })
        .collect();
    let mut born: Vec<(PathBuf, SystemTime)> = prev
        .map(|f| f.born.clone())
        .unwrap_or_default()
        .into_iter()
        .chain(new)
        .collect();
    born.sort_by_key(|(_, made)| *made);
    born.drain(..born.len().saturating_sub(BORN_KEPT));
    let folder = TaskFolder {
        path: folder.to_path_buf(),
        first: prev.map_or_else(SystemTime::now, |f| f.first),
        changed,
        names,
        born,
    };
    Some(Arc::new(folder))
}

/// `t` read on to the end of its whole lines: it as read, and its lines
/// new since, redacted. A file cut shorter is read from its start; one
/// gone is kept as it was.
fn read_on(t: Tracked, redactor: Option<&dyn Redactor>) -> (Tracked, Vec<String>) {
    let Some(len) = plain(&t.path).map(|m| m.len()) else {
        return (t, Vec::new());
    };
    let from = match len < t.read_to {
        true => 0,
        false => t.read_to,
    };
    let start = from.max(len.saturating_sub(TASK_TAIL));
    let mut raw = Vec::new();
    let read = std::fs::File::open(&t.path).and_then(|mut f| {
        f.seek(SeekFrom::Start(start))?;
        f.take(len - start).read_to_end(&mut raw)
    });
    let whole = raw.iter().rposition(|b| matches!(b, b'\n' | b'\r'));
    let (read_to, text) = match (read, whole) {
        (Ok(_), Some(end)) => (
            start + end as u64 + 1,
            String::from_utf8_lossy(&raw[..=end]).replace('\r', "\n"),
        ),
        (Ok(_), None) if start > from => (len, String::new()),
        _ => (from, String::new()),
    };
    let text = match start > from {
        true => text.split_once('\n').map_or("", |(_, rest)| rest),
        false => text.as_str(),
    };
    let ended = match text.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(last) => says_ended(last),
        None => t.ended,
    };
    let new = redacted_lines(text, redactor);
    let lines = t.lines.iter().chain(&new).cloned().collect::<Vec<_>>();
    let lines = lines[lines.len().saturating_sub(OUTPUT_LINES)..].to_vec();
    let t = Tracked {
        len,
        read_to,
        lines,
        ended,
        ..t
    };
    (t, new)
}

/// Whether `id` names a file of its own in a task folder.
fn plain_name(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
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

/// `path` in `folder` as first read: from its start when it was made since
/// the folder was first listed, from its end when it was there before.
fn first_read(folder: &TaskFolder, path: &Path) -> Option<Tracked> {
    let meta = plain(path)?;
    let from = match made(&meta)? >= folder.first {
        true => 0,
        false => meta.len(),
    };
    Some(Tracked::from(path.to_path_buf(), meta.created().ok(), from))
}

/// The output file `call` writes to in `folder`: the first made since it
/// started that is no background shell's.
fn call_file(call: &OpenCall, folder: &TaskFolder, shells: &[PathBuf]) -> Option<Tracked> {
    let (path, _) = folder
        .born
        .iter()
        .filter(|(p, made)| to_utc(*made) >= call.since - sight_procs::SKEW && !shells.contains(p))
        .min_by_key(|(_, made)| *made)?;
    first_read(folder, path)
}

/// What a session's files say since `prev`: each record's length and last
/// write, its open call (read again only when a record grew), and what its
/// open call and background shells (`shells`, by id) printed since.
fn read_files(
    records: &[PathBuf],
    engine: Engine,
    tasks: Option<PathBuf>,
    shells: &[String],
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
    let open_call = match prev.filter(|s| s.records == seen) {
        Some(p) => p.open_call.clone(),
        None => seen
            .iter()
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
            }),
    };
    let Some(folder) = tasks.and_then(|t| listed(&t, prev.and_then(|p| p.folder.as_ref()))) else {
        return Files {
            records: seen,
            open_call,
            ..Files::kept(None)
        };
    };
    let shells: Vec<(&String, PathBuf)> = shells
        .iter()
        .filter(|id| plain_name(id))
        .map(|id| (id, folder.path.join(format!("{id}.output"))))
        .collect();
    let shell_paths: Vec<PathBuf> = shells.iter().map(|(_, p)| p.clone()).collect();
    let held = |id: &str| {
        prev.into_iter()
            .flat_map(|p| p.call_output.iter().chain(&p.shells))
            .find(|(held, _)| held == id)
            .map(|(_, t)| t.clone())
    };
    let call_output = open_call
        .as_ref()
        .filter(|c| c.tool == SHELL_TOOL)
        .and_then(|c| {
            let t = held(&c.id).or_else(|| call_file(c, &folder, &shell_paths))?;
            Some((c.id.clone(), t))
        });
    let shells: Vec<Output> = shells
        .into_iter()
        .filter_map(|(id, path)| {
            let t = held(id).or_else(|| first_read(&folder, &path))?;
            Some((id.clone(), t))
        })
        .collect();
    let read = |(id, t): Output| {
        let (t, new) = read_on(t, redactor);
        ((id.clone(), t), (id, new))
    };
    let (call_output, call_new) = call_output.map(read).unzip();
    let (shells, shells_new): (Vec<Output>, Vec<_>) = shells.into_iter().map(read).unzip();
    Files {
        records: seen,
        open_call,
        folder: Some(folder),
        call_output,
        shells,
        new: call_new.into_iter().chain(shells_new).collect(),
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

    /// [`SightOptions::shell_unseen`].
    pub(super) fn shell_unseen(&self) -> chrono::Duration {
        chrono::Duration::from_std(self.0.opts.sight.shell_unseen).unwrap_or_default()
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
    /// older than [`SightOptions::fresh`] and a pass would sample it.
    pub async fn sample_fresh(&self, id: Uuid) -> Option<Sample> {
        let stale = || {
            self.sample(id).is_none_or(|s| {
                (Utc::now() - s.at).to_std().unwrap_or_default() >= self.0.opts.sight.fresh
            })
        };
        if stale() && self.sight_samples(id) {
            let _pass = self.0.sight.pass.lock().await;
            if stale() {
                self.sight_pass_held().await;
            }
        }
        self.sample(id)
    }

    /// The process a session's work runs under on this computer: its own
    /// process, an adopted one's, or the shell of the Desk terminal it was
    /// typed in. None when Desk knows of none.
    pub fn process_root(&self, id: Uuid) -> Option<u32> {
        let st = self.0.state.lock();
        let r = st.records.get(&id)?;
        Self::root_in(&st, r)
    }

    fn root_in(st: &super::State, r: &SessionRecord) -> Option<u32> {
        match st.live.get(&r.id) {
            Some(l) => Self::root_of(l, r.engine).or_else(|| l.terminal.and(l.pty.pid())),
            None => r.external_pid.and_then(|p| u32::try_from(p).ok()),
        }
    }

    /// Whether a pass samples `r`: working, or waiting on its own
    /// background work, and running here.
    fn sampled_by_pass(st: &super::State, r: &SessionRecord) -> bool {
        let background = st
            .live
            .get(&r.id)
            .is_some_and(|l| l.background.view().is_some());
        let working = matches!(
            r.state,
            SessionState::Starting | SessionState::Working | SessionState::Blocked
        );
        (background || working) && Self::root_in(st, r).is_some()
    }

    /// Whether a pass samples `id` now.
    fn sight_samples(&self, id: Uuid) -> bool {
        let st = self.0.state.lock();
        st.records
            .get(&id)
            .is_some_and(|r| Self::sampled_by_pass(&st, r))
    }

    /// The sessions a pass samples.
    fn sight_due(&self) -> Vec<Due> {
        let st = self.0.state.lock();
        st.records
            .values()
            .filter(|r| Self::sampled_by_pass(&st, r))
            .filter_map(|r| {
                let live = st.live.get(&r.id);
                Some(Due {
                    id: r.id,
                    record: r.clone(),
                    root: Self::root_in(&st, r)?,
                    baseline: live.and_then(|l| l.baseline.clone()),
                    shells: live
                        .map(|l| l.background.shells().map(str::to_string).collect())
                        .unwrap_or_default(),
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
        self.sight_pass_held().await;
    }

    /// [`Self::sight_pass`], its lock held.
    async fn sight_pass_held(&self) {
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
        self.shells_ended(&samples);
        self.sight_keep(samples);
    }

    /// Ends each background shell whose own output says it ended.
    fn shells_ended(&self, samples: &HashMap<Uuid, Sample>) {
        samples.iter().for_each(|(id, s)| {
            let ended: Vec<&str> = s
                .shells
                .iter()
                .filter(|(_, t)| t.ended)
                .map(|(shell, _)| shell.as_str())
                .collect();
            if !ended.is_empty() {
                self.change(*id, |_, l| {
                    if let Some(l) = l {
                        let now = Utc::now();
                        l.background = ended
                            .iter()
                            .fold(std::mem::take(&mut l.background), |b, shell| {
                                b.ended(shell, now)
                            });
                    }
                });
            }
        });
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
            let shells = d.shells.clone();
            let before = prev.cloned();
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
                    &shells,
                    before.as_ref(),
                    redactor.as_deref(),
                )
            })
            .await
            .unwrap_or_else(|_| Files::kept(prev))
        };
        let paths: Vec<PathBuf> = files.records.iter().map(|(p, ..)| p.clone()).collect();
        let kept_out = self.kept_out(&d.record, &paths, false).await.is_err();
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
            Some(p) if !sight_procs::work_active(&p.runs, &refs) => p.active_at,
            _ => now,
        };
        let last_write = files.records.iter().map(|(_, _, t)| to_utc(*t)).max();
        let grew_at = match prev {
            Some(p) if p.records == files.records => p.grew_at,
            Some(_) => now,
            None => last_write.unwrap_or(now),
        };
        let born: Vec<(&str, DateTime<Utc>)> = files
            .shells
            .iter()
            .filter_map(|(id, t)| Some((id.as_str(), t.born?)))
            .collect();
        let placed = sight_procs::placed(&refs, files.open_call.as_ref(), &born, now);
        let busy = |id: &str| {
            prev.zip(placed.get(id)).is_some_and(|(p, head)| {
                sight_procs::active(&p.runs, &sight_procs::subtree(&refs, head))
            })
        };
        let said: Vec<Said> = files
            .new
            .iter()
            .filter_map(|(id, new)| {
                let held = prev.and_then(|p| p.said.iter().find(|s| s.call == *id));
                sight_procs::said_after(held.cloned(), id, new, now, busy(id))
            })
            .collect();
        let reading = sight_procs::classify(&Evidence {
            tree: &refs,
            open_call: files.open_call.as_ref(),
            said: &said,
            placed: &placed,
            active_at,
            now,
            poll: chrono::Duration::from_std(self.0.opts.sight.poll).unwrap_or_default(),
        });
        fn same(r: &Reading) -> (&Kind, &str, Option<u32>, Option<&str>) {
            (&r.kind, &r.words, r.pid, r.call.as_deref())
        }
        let reading = match prev {
            Some(p) if same(&p.reading) == same(&reading) => Reading {
                since: p.reading.since.min(reading.since),
                ..reading
            },
            _ => reading,
        };
        let output = match kept_out {
            true => Vec::new(),
            false => {
                let screen = self
                    .terminal_text(d.id, SCREEN_CHARS)
                    .map(|t| redacted_lines(&t, redactor.as_deref()))
                    .unwrap_or_default();
                let call = files.call_output.iter().flat_map(|(_, t)| &t.lines);
                let shells = files.shells.iter().flat_map(|(_, t)| &t.lines);
                screen
                    .into_iter()
                    .chain(call.chain(shells).cloned())
                    .collect()
            }
        };
        let output_at = match prev {
            Some(p)
                if lengths(p.call_output.as_ref(), &p.shells)
                    == lengths(files.call_output.as_ref(), &files.shells) =>
            {
                p.output_at
            }
            _ => now,
        };
        let found = files
            .shells
            .iter()
            .map(|(id, _)| id)
            .filter(|id| placed.contains_key(*id))
            .cloned()
            .collect();
        let open_call = files.open_call.map(|c| match kept_out {
            true => OpenCall {
                input: String::new(),
                ..c
            },
            false => c,
        });
        Sample {
            at: now,
            first_at: prev.map_or(now, |p| p.first_at),
            listed: table.is_some(),
            root: d.root,
            waits: waits_of(&reading, now, self.0.opts.sight.shown_after),
            runs: sight_procs::runs(&refs),
            cpu_growing: growing,
            active_at,
            record_count: files.records.len(),
            records: files.records,
            grew_at,
            open_call,
            folder: files.folder,
            call_output: files.call_output,
            shells: files.shells,
            found,
            said,
            output,
            output_at,
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
        if !s.listed {
            return Processes::Unlisted;
        }
        let Some(root) = s.tree.first().map(|p| p.proc.pid) else {
            return Processes::NotRunning;
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

    /// What Desk sees inside `v` from its last sample, reading nothing:
    /// its folder and stall are for the caller to add.
    pub fn glance(&self, v: &crate::model::SessionView) -> Glance {
        let s = self.sample(v.id);
        let s = s.as_ref();
        let now = Utc::now();
        let o = &self.0.opts.sight;
        let lasted =
            |from: DateTime<Utc>, to: DateTime<Utc>| (to - from).to_std().unwrap_or_default();
        Glance {
            reading: s.map_or_else(|| Reading::quiet(now), |s| s.reading.clone()),
            open_call: s.and_then(|s| s.open_call.clone()),
            transcript: s.filter(|s| s.record_count > 0).map(|s| Growth {
                grew_at: s.grew_at,
                records: s.record_count,
            }),
            cpu_active_at: s.filter(|s| s.listed).map(|s| s.active_at),
            output_at: s.filter(|s| !s.output.is_empty()).map(|s| s.output_at),
            output: s.and_then(|s| s.output.last().cloned()),
            background: v.waiting_on(),
            settled: s.is_some_and(|s| lasted(s.first_at, s.at) >= o.settled),
            since: v.updated_at,
            as_of: s.map(|s| s.at),
            aged: s.is_some_and(|s| lasted(s.at, now) >= o.fresh),
            workspace: None,
            stall: None,
        }
    }

    /// `text` with its secrets hidden; none where they can't be.
    pub fn redact(&self, text: &str) -> Option<String> {
        self.0
            .ledger
            .redactor
            .as_ref()
            .map(|r| r.stored(text).text.into_owned())
    }

    /// A session's sampled wait, for [`Self::view_of`].
    pub(super) fn waits_in(st: &super::State, id: Uuid) -> Option<WaitsView> {
        st.sight.get(&id).and_then(|s| s.waits.clone())
    }
}

/// What the boss asks of a session's transcript: a page (before the cursor
/// a page gave), a search, one call's output, or a sub-agent's record.
#[derive(Debug, Clone, Default)]
pub struct TranscriptAsk {
    pub query: Option<String>,
    pub before: Option<String>,
    pub step: Option<String>,
    pub record: Option<String>,
}

/// One of a session's records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordInfo {
    pub name: String,
    pub bytes: u64,
    pub last_write: Option<DateTime<Utc>>,
}

/// What a transcript read gives.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Transcript {
    pub records: Vec<RecordInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exchanges: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hits: Option<Vec<Hit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<Call>,
    pub before: Option<String>,
    pub open_call: Option<OpenCall>,
}

/// A record file, read by range.
struct FileRanges {
    file: std::cell::RefCell<std::fs::File>,
    len: u64,
}

impl Ranges for FileRanges {
    fn size(&self) -> u64 {
        self.len
    }

    fn read_at(&self, at: u64, len: u64) -> std::io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(at))?;
        let mut buf = Vec::new();
        (&mut *file).take(len).read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// What tells one file from another that took its place: its device and
/// inode, else a hash of its start.
fn file_id(file: &mut std::fs::File) -> std::io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok(format!("{}-{}", meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        use sha2::Digest;
        use std::io::{Read, Seek, SeekFrom};
        let mut head = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        (&mut *file).take(4096).read_to_end(&mut head)?;
        Ok(hex::encode(&sha2::Sha256::digest(&head)[..12]))
    }
}

/// `path`, when it is a plain file inside `root` (canonical) and no link.
fn own_record(root: &Path, path: &Path) -> Option<PathBuf> {
    let plain = std::fs::symlink_metadata(path).ok()?.file_type().is_file();
    let real = path.canonicalize().ok()?;
    (plain && real.starts_with(root)).then_some(real)
}

/// A session's record's name: `main` for its own, a sub-agent's file stem.
fn record_name(i: usize, path: &Path) -> String {
    match i {
        0 => "main".into(),
        _ => path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// What `ask` reads of the record at `path`, an `engine` one.
fn read_transcript(
    path: &Path,
    engine: Engine,
    ask: &TranscriptAsk,
    redactor: &dyn Redactor,
) -> Result<Transcript, String> {
    let mut file = std::fs::File::open(path).map_err(|_| UNREAD.to_string())?;
    let id = file_id(&mut file).map_err(|_| UNREAD.to_string())?;
    let len = file.metadata().map_err(|_| UNREAD.to_string())?.len();
    let end = match &ask.before {
        None => len,
        Some(c) => transcript::read_cursor(c)
            .filter(|(of, at)| *of == id && *at <= len)
            .map(|(_, at)| at)
            .ok_or_else(|| REPLACED.to_string())?,
    };
    let r = FileRanges {
        file: std::cell::RefCell::new(file),
        len,
    };
    let budget = Budget {
        bytes: transcript::SCAN_BYTES,
        until: Some(std::time::Instant::now() + SCAN_FOR),
    };
    let unread = |_| UNREAD.to_string();
    let cursor = |at: Option<u64>| at.map(|at| transcript::cursor(&id, at));
    let empty = Transcript {
        records: Vec::new(),
        exchanges: Vec::new(),
        hits: None,
        step: None,
        before: None,
        open_call: None,
    };
    match (&ask.step, &ask.query) {
        (Some(step), _) => transcript::step(&r, engine, end, redactor, step, budget)
            .map_err(unread)?
            .map(|call| Transcript {
                step: Some(call),
                ..empty
            })
            .ok_or_else(|| STEP_UNSEEN.to_string()),
        (None, Some(query)) => {
            let found =
                transcript::search(&r, engine, end, redactor, query, budget).map_err(unread)?;
            Ok(Transcript {
                hits: Some(found.hits),
                before: cursor(found.before),
                ..empty
            })
        }
        (None, None) => {
            let page = transcript::page(&r, engine, end, redactor, budget).map_err(unread)?;
            Ok(Transcript {
                exchanges: page.exchanges,
                before: cursor(page.before),
                ..empty
            })
        }
    }
}

impl Desk {
    /// A page of `id`'s own transcript, newest first, a search of it, or
    /// one call's output, as `ask` says: read only where the person lets
    /// Desk read it and secrets can be hidden. Why not, in a sentence.
    pub async fn transcript_page(
        &self,
        id: Uuid,
        ask: TranscriptAsk,
    ) -> Result<Transcript, String> {
        let r = self
            .session_record(id)
            .await
            .ok_or_else(|| "There's no such session.".to_string())?;
        if self.0.opts.hub_internal.holds(Path::new(&r.cwd))
            || crate::ledger::agent_turn(&r.cwd, &r.name)
        {
            return Err(AGENT_TURN.into());
        }
        let records = self.records_blocking(&r).await;
        if records.is_empty() {
            return Err(format!(
                "{} keeps no transcript Desk can read.",
                r.engine.title()
            ));
        }
        let Some(redactor) = self.0.ledger.redactor.clone() else {
            return Err(NO_REDACTOR.into());
        };
        self.readable(&r, &records).await?;
        let path = match ask.record.as_deref() {
            None | Some("main") => records[0].clone(),
            Some(name) => records
                .iter()
                .enumerate()
                .skip(1)
                .find(|(i, p)| record_name(*i, p) == name)
                .map(|(_, p)| p.clone())
                .ok_or_else(|| NO_SUCH_RECORD.to_string())?,
        };
        let root = self
            .0
            .env
            .get("HOME")
            .and_then(|h| super::lifecycle::transcript_root(Path::new(h), r.engine))
            .and_then(|root| root.canonicalize().ok());
        let open_call = self.sample(id).and_then(|s| s.open_call);
        let engine = r.engine;
        tokio::task::spawn_blocking(move || {
            let path = root
                .and_then(|root| own_record(&root, &path))
                .ok_or_else(|| UNREAD.to_string())?;
            let infos = records
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let meta = std::fs::metadata(p).ok();
                    RecordInfo {
                        name: record_name(i, p),
                        bytes: meta.as_ref().map_or(0, std::fs::Metadata::len),
                        last_write: meta.and_then(|m| m.modified().ok()).map(to_utc),
                    }
                })
                .collect();
            read_transcript(&path, engine, &ask, redactor.as_ref()).map(|t| Transcript {
                records: infos,
                open_call,
                ..t
            })
        })
        .await
        .unwrap_or_else(|_| Err(UNREAD.into()))
    }

    /// `r`'s records, read off the runtime.
    async fn records_blocking(&self, r: &SessionRecord) -> Vec<PathBuf> {
        let desk = self.clone();
        let r = r.clone();
        tokio::task::spawn_blocking(move || desk.records_of(&r))
            .await
            .unwrap_or_default()
    }

    /// Whether the person lets Desk read `r`'s conversation, its records
    /// `records`; why not, in a sentence.
    async fn readable(&self, r: &SessionRecord, records: &[PathBuf]) -> Result<(), String> {
        self.kept_out(r, records, false)
            .await
            .map_err(|why| match why {
                Why::Skip(_) => KEPT_OUT.to_string(),
                Why::Hold(_) => UNCHECKED.to_string(),
            })
    }

    /// `id`'s own lines in Desk's logs around its turn, with `query` in
    /// them when asked, read only where the person lets Desk read its
    /// conversation; why not, in a sentence.
    pub async fn session_logs(&self, id: Uuid, query: Option<String>) -> Result<Logs, String> {
        let Some(dir) = self.0.opts.sight.logs.clone() else {
            return Err(NO_LOGS.into());
        };
        let Some(redactor) = self.0.ledger.redactor.clone() else {
            return Err(LOGS_NO_REDACTOR.into());
        };
        let r = self
            .session_record(id)
            .await
            .ok_or_else(|| "There's no such session.".to_string())?;
        let records = self.records_blocking(&r).await;
        self.readable(&r, &records).await?;
        let now = Utc::now();
        let start = self.state_since(id).unwrap_or(r.updated_at);
        let from = (start - LOGS_BEFORE).max(now - LOGS_MOST);
        tokio::task::spawn_blocking(move || {
            let own = r.id.to_string();
            let names: Vec<&str> = std::iter::once(own.as_str())
                .chain(r.cli_session_id.as_deref())
                .collect();
            let ask = logs::Ask {
                names: &names,
                from,
                to: now,
                query: query.as_deref(),
            };
            let lines = logs::FILES
                .iter()
                .filter_map(|f| {
                    let path = dir.join(f);
                    let plain = std::fs::symlink_metadata(&path).ok()?.file_type().is_file();
                    let text = plain
                        .then(|| crate::transcript::tail_of(&path, logs::TAIL).ok())
                        .flatten()?;
                    Some(logs::lines_of(f, &text, &ask, Some(redactor.as_ref())))
                })
                .flatten()
                .collect();
            logs::picked(lines, &ask)
        })
        .await
        .map_err(|_| "Desk's logs couldn't be read.".to_string())
    }
}

#[cfg(test)]
pub(super) fn redactor_for_tests() -> std::sync::Arc<dyn Redactor> {
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
    std::sync::Arc::new(Hides)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `path` as a file not yet read.
    fn unread(path: &Path) -> Tracked {
        Tracked::from(path.to_path_buf(), Some(SystemTime::now()), 0)
    }

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
    fn a_task_folder_is_listed_again_only_when_it_changed_and_never_follows_links() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("b-old.output");
        std::fs::write(&old, "all 3 build slots are busy\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let transcript = dir.path().join("agent.jsonl");
        std::fs::write(&transcript, "waiting for a review\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&transcript, dir.path().join("a-agent.output")).unwrap();
        let first = listed(dir.path(), None).unwrap();
        let born: Vec<&PathBuf> = first.born.iter().map(|(p, _)| p).collect();
        assert_eq!(
            born,
            [&old],
            "a sub-agent's linked transcript is never read"
        );
        let again = listed(dir.path(), Some(&first)).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(
            first_read(&first, &old).unwrap().read_to,
            27,
            "what was there before Desk looked is read from its end"
        );
        std::thread::sleep(Duration::from_millis(20));
        let new = dir.path().join("b-new.output");
        std::fs::write(&new, "all 3 build slots are busy\n").unwrap();
        let next = listed(dir.path(), Some(&first)).unwrap();
        assert_eq!(next.first, first.first);
        assert_eq!(first_read(&next, &new).unwrap().read_to, 0, "made since");
        assert_eq!(next.born.len(), 2);
        assert!(next.names.len() == 5 && next.born.iter().any(|(p, _)| *p == new));
    }

    fn append_to(path: &Path, text: &str) {
        let mut f = std::fs::File::options().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut f, text.as_bytes()).unwrap();
    }

    #[test]
    fn a_task_file_is_read_on_from_where_it_was_left_in_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b1.output");
        std::fs::write(&path, "one\nkey SECRET-abc\npart").unwrap();
        let r = redactor_for_tests();
        let (t, new) = read_on(unread(&path), Some(r.as_ref()));
        assert_eq!(new, ["one", "key [secret]"]);
        assert_eq!((t.read_to, t.len), (19, 23));
        append_to(&path, "ial\rwaiting for one\n");
        let (t, new) = read_on(t, Some(r.as_ref()));
        assert_eq!(new, ["partial", "waiting for one"]);
        assert_eq!(
            t.lines,
            ["one", "key [secret]", "partial", "waiting for one"]
        );
        let (t, new) = read_on(t, Some(r.as_ref()));
        assert!(new.is_empty(), "nothing new since");
        std::fs::write(&path, "again\n").unwrap();
        let (t, new) = read_on(t, Some(r.as_ref()));
        assert_eq!((new, t.read_to), (vec!["again".to_string()], 6));
        assert!(!t.ended);
        append_to(&path, "done\n\n[exited with code 0]\n");
        let (t, _) = read_on(t, Some(r.as_ref()));
        assert!(t.ended, "Claude Code's last line says its command ended");
        let (t, new) = read_on(t, Some(r.as_ref()));
        assert!(t.ended && new.is_empty(), "and it stays ended");
        let (bare, lines) = read_on(unread(&path), None);
        assert!(lines.is_empty(), "nothing is read without a redactor");
        assert!(bare.ended, "but its end is");
        std::fs::write(&path, "[killed]\n").unwrap();
        assert!(read_on(unread(&path), None).0.ended);
        std::fs::write(&path, "echo [exited with code 0] means done\n").unwrap();
        assert!(!read_on(unread(&path), None).0.ended);
        let big = dir.path().join("b2.output");
        let text = format!("{}\nlast\n", "x".repeat(TASK_TAIL as usize));
        std::fs::write(&big, text).unwrap();
        let (_, new) = read_on(unread(&big), Some(r.as_ref()));
        assert_eq!(new, ["last"], "a line cut by the tail is left out");
    }

    #[test]
    fn an_open_calls_file_is_the_first_made_since_it_started_and_no_shells() {
        let dir = tempfile::tempdir().unwrap();
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let file = |name: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, "printed before\n").unwrap();
            p
        };
        let (before, shell, own, later) = (file("b0"), file("b1"), file("b2"), file("b3"));
        let folder = TaskFolder {
            path: dir.path().to_path_buf(),
            first: at(0),
            changed: at(0),
            names: HashSet::new(),
            born: vec![
                (before, at(990)),
                (shell.clone(), at(1001)),
                (own.clone(), at(1002)),
                (later.clone(), at(1005)),
            ],
        };
        let call = OpenCall {
            id: "toolu_1".into(),
            tool: "Bash".into(),
            input: String::new(),
            since: to_utc(at(1000)),
        };
        let made_since = call_file(&call, &folder, std::slice::from_ref(&shell)).unwrap();
        assert_eq!(
            (made_since.path.as_path(), made_since.read_to),
            (own.as_path(), 0)
        );
        let listed_later = TaskFolder {
            first: SystemTime::now() + Duration::from_secs(3600),
            ..folder
        };
        let old = call_file(&call, &listed_later, &[shell]).unwrap();
        assert_eq!(old.read_to, 15, "a file there before is read from its end");
        let late = OpenCall {
            since: to_utc(at(1100)),
            ..call
        };
        assert!(call_file(&late, &listed_later, &[]).is_none());
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
            root: 1,
            runs: HashMap::new(),
            cpu_growing: HashSet::from([3]),
            active_at: now,
            records: vec![],
            grew_at: now,
            record_count: 0,
            open_call: None,
            folder: None,
            call_output: None,
            shells: vec![],
            found: BTreeSet::new(),
            said: vec![],
            output: vec![],
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
