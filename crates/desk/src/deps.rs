//! A folder's JS dependencies, installed for a plan's ticket session as it
//! starts, so it never stops for want of them.
//!
//! - **Only the repository's own tool:** the one its `packageManager` names
//!   (pnpm, npm, yarn or bun, at a strict semver version), else the one its
//!   only lockfile is; anything else is said, never run.
//! - **Frozen, no root:** `npx --yes --package=<tool>@<version> -- <tool>`
//!   with its frozen install (`npm ci` with the machine's own npm when
//!   nothing pins it), given as an argv list, never through a shell, in the
//!   environment a clone gets. Each tool's store and cache under the home
//!   are shared by every folder it installs into.
//! - **The repository's own code:** an install runs what the repository
//!   says it runs (its packages' install scripts, its `.npmrc`, a pnpmfile,
//!   Berry's `yarnPath` and plugins), so Desk runs one only for a start
//!   that asks: a plan's ticket, its folder vouched for by the plan's
//!   approval, or a folder the person chose.
//! - **Read with care:** `package.json` and the lockfile are read only as
//!   plain files in the folder, never through a link, and only so much.
//! - **Once:** a folder whose install is current is left as it is: Desk's
//!   stamp in `node_modules` matches, or, with none, the tool's own state is
//!   no older than the lockfile. A failed install stamps that it failed, so
//!   the tool's state isn't trusted until one ends well.
//! - **Said in Desk's words:** what failed is told plainly, never in the
//!   tool's own output, which goes to the log.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::run::Ran;

/// How long one install may take.
pub const INSTALL_FOR: Duration = Duration::from_secs(15 * 60);
/// The variable each install's processes carry its run's token in, so one
/// left running is told apart from any other.
const RUN_VAR: &str = "DESK_DEPS_RUN";
/// The folder Desk keeps its stamp in, whichever the tool.
const STATE: &str = "node_modules";
/// Desk's stamp of the install it made.
const STAMP: &str = ".desk-deps";
/// What the stamp says of an install that didn't end well.
const FAILED: &str = "failed";
/// The most of a package.json read.
const MOST_MANIFEST: u64 = 1 << 20;
/// The most of a lockfile read.
const MOST_LOCK: u64 = 64 << 20;
/// The most of an install's output a failure logs.
const MOST_LOGGED: usize = 2000;
/// Each lockfile, and the tool it is.
const LOCKS: &[(&str, Tool)] = &[
    ("pnpm-lock.yaml", Tool::Pnpm),
    ("npm-shrinkwrap.json", Tool::Npm),
    ("package-lock.json", Tool::Npm),
    ("yarn.lock", Tool::Yarn),
    ("bun.lock", Tool::Bun),
    ("bun.lockb", Tool::Bun),
];

/// A package manager Desk runs: Yarn 1 (`Yarn`) and Yarn 2 or later
/// (`Berry`) apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pnpm,
    Npm,
    Yarn,
    Berry,
    Bun,
}

impl Tool {
    fn named(name: &str) -> Option<Self> {
        match name {
            "pnpm" => Some(Self::Pnpm),
            "npm" => Some(Self::Npm),
            "yarn" => Some(Self::Yarn),
            "bun" => Some(Self::Bun),
            _ => None,
        }
    }

    /// As `version` makes it: Yarn 2 or later is Berry.
    fn at(self, version: &str) -> Self {
        let major = version
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        match (self, major) {
            (Self::Yarn, Some(m)) if m >= 2 => Self::Berry,
            (t, _) => t,
        }
    }

    /// The tool its lockfile is.
    fn family(self) -> Self {
        match self {
            Self::Berry => Self::Yarn,
            t => t,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm",
            Self::Npm => "npm",
            Self::Yarn | Self::Berry => "yarn",
            Self::Bun => "bun",
        }
    }

    fn package(self) -> &'static str {
        match self {
            Self::Berry => "@yarnpkg/cli-dist",
            t => t.name(),
        }
    }

    fn install(self) -> &'static [&'static str] {
        match self {
            Self::Npm => &["ci"],
            Self::Berry => &["install", "--immutable"],
            Self::Pnpm | Self::Yarn | Self::Bun => &["install", "--frozen-lockfile"],
        }
    }

    /// The file its own install writes last, in the folder, when it writes
    /// one.
    fn marker(self) -> Option<&'static str> {
        match self {
            Self::Pnpm => Some("node_modules/.modules.yaml"),
            Self::Npm => Some("node_modules/.package-lock.json"),
            Self::Yarn => Some("node_modules/.yarn-integrity"),
            Self::Berry => Some(".yarn/install-state.gz"),
            Self::Bun => None,
        }
    }
}

/// An install Desk runs in a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    tool: Tool,
    /// The version run (`9.15.9`, or a major, `9`); none: the machine's own.
    version: Option<String>,
    /// The lockfile it keeps to.
    lock: &'static str,
    /// What Desk's stamp of it says: its lockfile as it was read, and how it
    /// is installed.
    print: String,
}

impl Install {
    fn new(tool: Tool, version: Option<String>, lock: &'static str, bytes: &[u8]) -> Self {
        let mut i = Self {
            tool,
            version,
            lock,
            print: String::new(),
        };
        i.print = format!("{} {}", hex::encode(Sha256::digest(bytes)), i.command());
        i
    }

    /// Its words, the program first.
    pub fn argv(&self) -> Vec<String> {
        let run = std::iter::once(self.tool.name())
            .chain(self.tool.install().iter().copied())
            .map(str::to_string);
        match &self.version {
            Some(v) => [
                "npx".to_string(),
                "--yes".to_string(),
                format!("--package={}@{v}", self.tool.package()),
                "--".to_string(),
            ]
            .into_iter()
            .chain(run)
            .collect(),
            None => run.collect(),
        }
    }

    /// Its words as one command, as the session is shown it to run.
    pub fn command(&self) -> String {
        self.argv().join(" ")
    }

    /// Whether its install in `dir` is current, as the module says: never
    /// through a linked `node_modules`.
    fn current(&self, dir: &Path) -> bool {
        if std::fs::symlink_metadata(dir.join(STATE)).is_ok_and(|m| !m.is_dir()) {
            return false;
        }
        let modified = |p: &Path| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok();
        // The tool's own state, made no earlier than the lockfile, and after
        // `since` when given.
        let marked = |since: Option<std::time::SystemTime>| {
            self.tool.marker().is_some_and(|m| {
                modified(&dir.join(m))
                    .zip(modified(&dir.join(self.lock)))
                    .is_some_and(|(made, locked)| made >= locked && since.is_none_or(|s| made > s))
            })
        };
        let stamp = dir.join(STATE).join(STAMP);
        match read_in(&dir.join(STATE), STAMP, 4096) {
            Ok(Some(had)) if had == self.print.as_bytes() => true,
            Ok(Some(had)) if had == FAILED.as_bytes() => {
                modified(&stamp).is_some_and(|failed| marked(Some(failed)))
            }
            Ok(Some(_)) | Err(_) => false,
            Ok(None) => marked(None),
        }
    }
}

/// `name` in `dir`, read when it is a plain file there (never through a
/// link) of at most `most` bytes; none when it isn't there.
fn read_in(dir: &Path, name: &str, most: u64) -> Result<Option<Vec<u8>>, String> {
    let unread = || format!("its {name} can't be read");
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(name));
    let file = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(format!("its {name} is a link, so Desk reads none of it"))
        }
        Err(_) => return Err(unread()),
    };
    let meta = file.metadata().map_err(|_| unread())?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "its {name} isn't a plain file, so Desk reads none of it"
        ));
    }
    if meta.len() > most {
        return Err(format!("its {name} is larger than Desk reads"));
    }
    let mut bytes = Vec::new();
    file.take(most)
        .read_to_end(&mut bytes)
        .map_err(|_| unread())?;
    Ok(Some(bytes))
}

/// What Desk found of a folder's dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Current(Install),
    Stale(Install),
    /// Desk installs nothing there: why.
    Left(String),
}

/// What Desk found of `dir`'s dependencies; none when it has no
/// `package.json`, and nothing is said of them.
pub fn found(dir: &Path) -> Option<Found> {
    let manifest = match read_in(dir, "package.json", MOST_MANIFEST) {
        Ok(None) => return None,
        Ok(Some(bytes)) => Ok(bytes),
        Err(why) => Err(why),
    };
    let found = manifest
        .and_then(|b| {
            serde_json::from_slice::<Value>(&b)
                .map_err(|_| "its package.json can't be read".to_string())
        })
        .and_then(|m| chosen(dir, &m))
        .map_or_else(Found::Left, |i| match i.current(dir) {
            true => Found::Current(i),
            false => Found::Stale(i),
        });
    Some(found)
}

/// The install `dir`'s `manifest` and lockfiles name, as the module says.
fn chosen(dir: &Path, manifest: &Value) -> Result<Install, String> {
    let mut locks: Vec<(&'static str, Tool)> = LOCKS
        .iter()
        .filter(|(f, _)| std::fs::symlink_metadata(dir.join(f)).is_ok())
        .copied()
        .collect();
    locks.dedup_by_key(|(_, t)| *t);
    let read = |lock: &str| {
        read_in(dir, lock, MOST_LOCK)?.ok_or_else(|| format!("its {lock} went as Desk read it"))
    };
    match manifest.get("packageManager").filter(|v| !v.is_null()) {
        Some(said) => {
            let (tool, version) = pinned(said)?;
            let lock = locks
                .iter()
                .find(|(_, t)| *t == tool.family())
                .map(|(f, _)| *f)
                .ok_or_else(|| {
                    let files: Vec<&str> = LOCKS
                        .iter()
                        .filter(|(_, t)| *t == tool.family())
                        .map(|(f, _)| *f)
                        .collect();
                    format!(
                        "its package.json names {} but there's no {}",
                        tool.name(),
                        files.join(" or ")
                    )
                })?;
            Ok(Install::new(tool, Some(version), lock, &read(lock)?))
        }
        None => match *locks.as_slice() {
            [] => Err("it has no lockfile".into()),
            [(lock, tool)] => {
                let bytes = read(lock)?;
                Ok(Install::new(tool, unpinned(tool, &bytes)?, lock, &bytes))
            }
            _ => Err(
                "it has lockfiles of several package managers and package.json names none".into(),
            ),
        },
    }
}

/// The tool and version `packageManager` says, when it says them as Desk
/// runs them: `name@version`, its `+<hash>` dropped.
fn pinned(said: &Value) -> Result<(Tool, String), String> {
    let refused =
        || "its package.json's packageManager isn't a name and version Desk runs".to_string();
    let (name, version) = said
        .as_str()
        .and_then(|s| s.trim().split_once('@'))
        .ok_or_else(refused)?;
    let version = version.split_once('+').map_or(version, |(v, _)| v);
    Tool::named(name)
        .filter(|_| semver(version))
        .map(|t| (t.at(version), version.to_string()))
        .ok_or_else(refused)
}

/// Whether `v` is a version and nothing else: `1.2.3`, or `1.2.3-rc.1`.
fn semver(v: &str) -> bool {
    let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p)));
    let parts: Vec<&str> = core.split('.').collect();
    v.len() <= 64
        && parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && pre.is_none_or(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        })
}

/// The version of `tool` run when nothing pins it, from its lockfile's
/// first lines.
fn unpinned(tool: Tool, lock: &[u8]) -> Result<Option<String>, String> {
    let head = String::from_utf8_lossy(&lock[..lock.len().min(1024)]);
    match tool {
        Tool::Npm => Ok(None),
        Tool::Bun => Ok(Some("1".into())),
        Tool::Yarn | Tool::Berry => match head.contains("# yarn lockfile v1") {
            true => Ok(Some("1".into())),
            false => {
                Err("its yarn.lock is Yarn 2 or later, and package.json doesn't say which".into())
            }
        },
        Tool::Pnpm => {
            let said = head
                .lines()
                .find_map(|l| l.strip_prefix("lockfileVersion:"))
                .map(|v| v.trim().trim_matches(['\'', '"']).to_string())
                .unwrap_or_default();
            let major = said.split('.').next().and_then(|m| m.parse::<u32>().ok());
            match (major, said.as_str()) {
                (Some(9), _) => Ok(Some("9".into())),
                (Some(6), _) => Ok(Some("8".into())),
                (_, "5.4") => Ok(Some("7".into())),
                _ => Err("its pnpm-lock.yaml is of a version Desk doesn't know, and package.json doesn't name pnpm's".into()),
            }
        }
    }
}

/// Where Desk keeps which process installs into `dir`, in its `run_dir`.
pub fn running_in(run_dir: &Path, dir: &Path) -> PathBuf {
    let key = hex::encode(Sha256::digest(dir.as_os_str().as_bytes()));
    run_dir.join(format!("deps-{}.pid", &key[..16]))
}

/// Runs `i` in `dir` as the module says, for at most `wait`: the machine's
/// `home`, and `env` the clone's ([`crate::repos::kept_env`]). `running`
/// keeps which process group runs it, so one a Desk stopped since left
/// running there is ended before another begins. What failed, in Desk's
/// words.
pub async fn install(
    i: &Install,
    dir: &Path,
    home: &Path,
    env: &[(String, String)],
    wait: Duration,
    running: &Path,
) -> Result<(), String> {
    let argv = i.argv();
    let Some(program) = crate::repos::program(&argv[0], home, env) else {
        return Err(match i.version {
            Some(_) => {
                "npx isn't installed on this machine: Desk needs node and npm to install them"
            }
            None => "npm isn't installed on this machine",
        }
        .into());
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    let env: Vec<(String, String)> = env
        .iter()
        .cloned()
        .chain(
            [
                ("HOME", home.to_string_lossy().as_ref()),
                (RUN_VAR, &token),
                // No terminal: pnpm replaces a node_modules without asking.
                ("CI", "1"),
                // A corepack shim refuses another manager's project.
                ("COREPACK_ENABLE_STRICT", "0"),
                ("npm_config_update_notifier", "false"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        )
        .collect();
    end_left(running).await;
    let (began, pid) = tokio::sync::oneshot::channel::<u32>();
    let keep = async {
        if let Ok(pid) = pid.await {
            let since = crate::procs::started_at(pid).await;
            let since = since.map_or_else(|| "-".to_string(), |s| s.to_rfc3339());
            let kept = format!("{pid} {since} {token}");
            if let Some(parent) = running.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(e) = tokio::fs::write(running, kept).await {
                tracing::info!(folder = %dir.display(), error = %e, "which process installs a folder's dependencies wasn't kept");
            }
        }
    };
    let (ran, ()) = tokio::join!(
        crate::run::exec(&program, &argv[1..], dir, wait, &env, move |p| {
            let _ = began.send(p);
        }),
        keep
    );
    let _ = tokio::fs::remove_file(running).await;
    let ended = match ran {
        Ran::Ran {
            exit: Some(0),
            timed_out: false,
            ..
        } => Ok(()),
        Ran::Ran {
            timed_out: true, ..
        } => Err(match wait.as_secs().div_ceil(60) {
            0 | 1 => "it took longer than a minute".to_string(),
            n => format!("it took longer than {n} minutes"),
        }),
        Ran::Ran {
            exit,
            stdout,
            stderr,
            ..
        } => {
            let said = format!("{stdout}\n{stderr}");
            let tail: String = said
                .chars()
                .rev()
                .take(MOST_LOGGED)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            tracing::warn!(folder = %dir.display(), command = %i.command(), ?exit, output = %tail, "a folder's dependencies didn't install");
            Err(failure(&stdout, &stderr, exit))
        }
        Ran::Refused { why } | Ran::NeedsPerson { why } => Err(why),
    };
    match &ended {
        Ok(()) => stamp(dir, &i.print, true).await,
        Err(_) => stamp(dir, FAILED, false).await,
    }
    ended
}

/// Ends the install `running` says a stopped Desk left running: its
/// process group, when a process of it carries that install's token in its
/// environment, and its leader, when there, is still the process that began
/// then. One Desk can't tell is that install is left, and said so.
pub async fn end_left(running: &Path) {
    let Ok(kept) = tokio::fs::read_to_string(running).await else {
        return;
    };
    let _ = tokio::fs::remove_file(running).await;
    let mut words = kept.split_whitespace();
    let Some((group, pgid)) = words
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .and_then(|p| i32::try_from(p).ok().filter(|g| *g > 1).map(|g| (p, g)))
    else {
        return;
    };
    let then = words
        .next()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.to_utc());
    let mark = words.next().map(|t| format!("{RUN_VAR}={t}"));
    // SAFETY: getpgid only reads a process's group.
    let in_group = |pid: u32| i32::try_from(pid).is_ok_and(|p| unsafe { libc::getpgid(p) } == pgid);
    // The process that shows the group is the install's.
    let ours = match (mark, crate::procs::started_at(group).await) {
        (None, _) => None,
        (Some(mark), Some(leader)) => {
            let began = then.is_some_and(|t| (leader - t).num_seconds().abs() <= 2);
            (began && crate::procs::carries(group, &mark).await).then_some(group)
        }
        (Some(mark), None) => {
            let mut found = None;
            for pid in crate::procs::in_group(group).await {
                if in_group(pid) && crate::procs::carries(pid, &mark).await {
                    found = Some(pid);
                    break;
                }
            }
            found
        }
    };
    if ours.is_some_and(in_group) {
        tracing::info!(group, "an install a stopped Desk left running is ended");
        // SAFETY: killpg only signals the group the install made, one of
        // its own still in it as checked just now.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    // SAFETY: signal 0 only asks whether the group is there.
    } else if unsafe { libc::killpg(pgid, 0) } == 0 {
        tracing::warn!(group, "a process group where an install a stopped Desk left was running isn't ended: Desk can't tell it is that install");
    }
}

/// Desk's stamp in `dir`, saying `what`: written beside the stamp and moved
/// over it, so a link there is replaced, never written through; never in a
/// `node_modules` that is a link, nor in one there isn't unless `make`.
async fn stamp(dir: &Path, what: &str, make: bool) {
    let state = dir.join(STATE);
    match tokio::fs::symlink_metadata(&state).await {
        Ok(m) if m.is_dir() => {}
        Err(_) if make => {
            if tokio::fs::create_dir(&state).await.is_err() {
                return;
            }
        }
        Ok(_) | Err(_) => return,
    }
    let part = state.join(format!("{STAMP}.{}", uuid::Uuid::new_v4().simple()));
    let written = async {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut f, what.as_bytes()).await?;
        tokio::fs::rename(&part, state.join(STAMP)).await
    };
    if let Err(e) = written.await {
        let _ = tokio::fs::remove_file(&part).await;
        tracing::info!(folder = %dir.display(), error = %e, "an install's stamp wasn't written: the next start installs again");
    }
}

/// Each kind of failure: the tool's own codes for it, the plain words that
/// say it on stderr, and Desk's words for it.
const FAILURES: &[(&[&str], &[&str], &str)] = &[
    (
        &[
            "ERR_PNPM_OUTDATED_LOCKFILE",
            "ERR_PNPM_LOCKFILE_CONFIG_MISMATCH",
            "code EUSAGE",
            "YN0028",
        ],
        &[
            "frozen-lockfile",
            "are in sync",
            "lockfile needs to be updated",
        ],
        "its lockfile doesn't match its package.json",
    ),
    (
        &[
            "ERR_PNPM_FETCH_401",
            "ERR_PNPM_FETCH_403",
            "code E401",
            "code E403",
            "YN0041",
        ],
        &["unauthorized", "authentication", "forbidden"],
        "the package registry refused this machine's sign-in",
    ),
    (
        &[
            "ERR_PNPM_META_FETCH_FAIL",
            "code ENOTFOUND",
            "code ETIMEDOUT",
            "code ECONNRESET",
            "code ECONNREFUSED",
            "code EAI_AGAIN",
        ],
        &["network", "proxy", "socket hang up", "getaddrinfo"],
        "the network didn't reach the package registry from this machine",
    ),
    (
        &["ENOSPC"],
        &["no space left"],
        "this machine's disk is full",
    ),
    (
        &[
            "ERR_PNPM_EACCES",
            "ERR_PNPM_EPERM",
            "code EACCES",
            "code EPERM",
        ],
        &["permission denied"],
        "it wasn't allowed to write where it installs",
    ),
    (
        &["ERR_PNPM_UNSUPPORTED_ENGINE", "code EBADENGINE"],
        &["unsupported engine", "incompatible with this module"],
        "this machine's node isn't a version the repository supports",
    ),
    (
        &[
            "ELIFECYCLE",
            "ERR_PNPM_LIFECYCLE",
            "YN0009",
            "npm error command failed",
            "npm ERR! command failed",
        ],
        &["lifecycle script"],
        "one of its packages' install scripts failed",
    ),
];

/// Why an install that ended `exit` failed, from what it said: its tool's
/// codes first, then plain words on `stderr`. In Desk's words alone, as what
/// a registry or a package's script says isn't said on.
fn failure(stdout: &str, stderr: &str, exit: Option<i32>) -> String {
    let said = format!("{stdout}\n{stderr}");
    let err = stderr.to_lowercase();
    FAILURES
        .iter()
        .find(|(codes, _, _)| codes.iter().any(|c| said.contains(c)))
        .or_else(|| {
            FAILURES
                .iter()
                .find(|(_, words, _)| words.iter().any(|w| err.contains(w)))
        })
        .map_or_else(
            || match exit {
                Some(127) => {
                    "a program it runs isn't installed on this machine (node, for npx)".into()
                }
                Some(code) => format!("it ended with exit {code}"),
                None => "it was ended before it finished".into(),
            },
            |(_, _, why)| (*why).to_string(),
        )
}

/// What a session is told of its folder's dependencies, and its card shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum Deps {
    Installing { command: String },
    Installed { command: String },
    Failed { command: String, why: String },
    Left { why: String },
}

impl Deps {
    /// As a session starts on what Desk `found`.
    pub fn begun(found: &Found) -> Self {
        match found {
            Found::Current(i) => Self::Installed {
                command: i.command(),
            },
            Found::Stale(i) => Self::Installing {
                command: i.command(),
            },
            Found::Left(why) => Self::Left { why: why.clone() },
        }
    }

    /// How an install of `command` ended.
    pub fn ended(command: String, ended: Result<(), String>) -> Self {
        match ended {
            Ok(()) => Self::Installed { command },
            Err(why) => Self::Failed { command, why },
        }
    }

    /// Whether it is over and only said: failed, or left.
    pub fn said_only(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Left { .. })
    }

    /// What the session's brief ends with.
    pub fn brief(&self) -> String {
        match self {
            Self::Installing { command } => format!(
                "Desk is installing this folder's dependencies now, with `{command}`, and tells you here when they're in. \
                 If you need them before that, run `{command}` yourself."
            ),
            Self::Installed { command } => format!(
                "This folder's dependencies are installed (`{command}`): run its tests and type checks as you go."
            ),
            Self::Failed { command, why } => format!(
                "Desk couldn't install this folder's dependencies: {why}. Run `{command}` in this folder, then its \
                 tests and type checks."
            ),
            Self::Left { why } => format!(
                "Desk didn't install this folder's dependencies: {why}. Install them yourself with the repository's own \
                 package manager, without root (`npx --yes --package=<tool>@<version> -- <tool> install --frozen-lockfile`, \
                 or `npm ci`), before you run its tests."
            ),
        }
    }

    /// What the session is told once its install ended.
    pub fn told(&self) -> String {
        match self {
            Self::Installed { command } => format!(
                "Desk installed this folder's dependencies (`{command}`): you can run its tests and type checks now."
            ),
            other => other.brief(),
        }
    }
}

#[cfg(test)]
#[path = "deps_tests.rs"]
mod tests;
