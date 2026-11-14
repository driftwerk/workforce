//! Charges: which boss runs which session. A session in a boss's charge is
//! one the boss orchestrates over its line, is woken for at each turn's end,
//! and answers for. The person can always take the wheel: typing in its
//! terminal, prompting it or writing to it puts it in their hands, and the
//! boss's messages wait until it comes back, by click or once they have
//! given it no input for the hold ([`HOLD`] unless the boss's settings say
//! otherwise).
//!
//! Desk says "take on" and "release" here: picking up a session from
//! another terminal is [`crate::adopt`]'s, and is something else.
//!
//! This module decides; [`crate::service`]'s `charge` submodule stores and
//! announces.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use serde_json::Value;

use crate::computer::boss;
use crate::computer::policy::sends_to_people;
use crate::engine::{Deliver, Engine};
use crate::model::{text_enum, Origin, PendingApproval, Request, SessionState, SessionView};
use crate::scope::{sees, Scope};

text_enum!(How {
    // The boss started it.
    Started => "started",
    // The person handed it to the boss.
    Handed => "handed",
    // The boss took it on by itself: it takes on every session.
    Taken => "taken",
});

/// Who drives a charged session now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wheel {
    Boss,
    /// The person: back to the boss once they have given the session no
    /// input for the hold ([`HOLD`] by default).
    Person,
    /// The person, until they hand it back.
    PersonPinned,
}

impl Wheel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boss => "boss",
            Self::Person => "person",
            Self::PersonPinned => "person_pinned",
        }
    }

    pub fn parse(s: &str) -> eyre::Result<Self> {
        match s {
            "boss" => Ok(Self::Boss),
            "person" => Ok(Self::Person),
            "person_pinned" => Ok(Self::PersonPinned),
            other => Err(eyre::eyre!("unknown Wheel {other:?}")),
        }
    }

    /// Whether the person has it.
    pub fn person(self) -> bool {
        self != Self::Boss
    }
}

/// How long the person's wheel lasts after their last input (a keystroke,
/// a prompt, a message) before it goes back to the boss, unless the boss's
/// `[oversight.wake] hold_minutes` says otherwise.
pub const HOLD: chrono::Duration = chrono::Duration::minutes(2);

/// How often, at most, Desk's window says the person is typing in a
/// session's composer (`POST /api/desk/sessions/{id}/typing`): well inside
/// the shortest hold.
pub const TYPING_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

/// One session's charge, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charge {
    pub session: Uuid,
    /// The boss's sidebar group; none is the main boss.
    pub group: Option<String>,
    /// The boss's agent id.
    pub boss: String,
    pub how: How,
    pub wheel: Wheel,
    /// When the wheel last changed hands.
    pub wheel_since: Option<DateTime<Utc>>,
    pub since: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl Charge {
    /// A new charge of `session` to `boss`, now, the boss at the wheel.
    pub fn new(session: Uuid, boss: &str, how: How, group: Option<String>) -> Self {
        let now = crate::crew::now();
        Self {
            session,
            group,
            boss: boss.to_string(),
            how,
            wheel: Wheel::Boss,
            wheel_since: None,
            since: now,
            released_at: None,
            updated_at: now,
        }
    }

    /// Whether a boss runs it now: it hasn't been released.
    pub fn live(&self) -> bool {
        self.released_at.is_none()
    }
}

/// Why a session can't be taken on, in the words the person reads.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotChargeable {
    #[error("It runs in another terminal. Continue it in Desk first.")]
    External,
    /// Its engine (or the version it runs) has no line: what to do about
    /// it, as Desk's notes say.
    #[error("{0}")]
    NoLine(String),
    #[error("A custom command has no line for a boss to talk on.")]
    Custom,
    /// It is in a group with a boss of its own.
    #[error("It's in {0}, which has a boss of its own.")]
    OtherScope(String),
    /// It is outside the group the boss runs.
    #[error("It isn't in the group you oversee.")]
    Outside,
    #[error("It has ended.")]
    Ended,
}

/// Whether `s` can be taken on by the boss of `scope`, the groups in
/// `bossed` each having a boss of its own: it needs a line from Desk, and
/// to be in that boss's scope.
pub fn chargeable(
    s: &SessionView,
    scope: &Scope,
    bossed: &HashSet<String>,
) -> Result<(), NotChargeable> {
    if s.origin == Origin::External {
        return Err(NotChargeable::External);
    }
    if s.state == SessionState::Closed {
        return Err(NotChargeable::Ended);
    }
    if s.engine == Engine::Custom {
        return Err(NotChargeable::Custom);
    }
    if s.caps.line == Deliver::None {
        let why = s.caps.notes.first().map_or_else(
            || format!("{} sessions have no line from Desk yet.", s.engine.title()),
            |n| n.to_string(),
        );
        return Err(NotChargeable::NoLine(why));
    }
    if sees(scope, s.group.as_ref().map(|g| g.id.as_str()), bossed) {
        return Ok(());
    }
    match &s.group {
        Some(g) if bossed.contains(&g.id) => Err(NotChargeable::OtherScope(g.name.clone())),
        _ => Err(NotChargeable::Outside),
    }
}

/// Whether a boss that takes on every session (`all`) takes `s` on now: it
/// can be, it isn't charged already, and the person never released it.
pub fn takes_automatically(
    all: bool,
    s: &SessionView,
    existing: Option<&Charge>,
    scope: &Scope,
    bossed: &HashSet<String>,
) -> bool {
    all && existing.is_none() && chargeable(s, scope, bossed).is_ok()
}

/// How many of `charges` hold one of their boss's slots: starting or
/// working.
pub fn busy(charges: &[Charge], states: &HashMap<Uuid, SessionState>) -> usize {
    charges
        .iter()
        .filter(|c| c.live())
        .filter(|c| {
            matches!(
                states.get(&c.session),
                Some(SessionState::Starting | SessionState::Working)
            )
        })
        .count()
}

/// Who has the wheel of `c` at `now`, given when the person last gave the
/// session input (typed in its terminal, prompted it, wrote to it) and how
/// long that input holds it (`hold`). Only input since the boss last had
/// the wheel counts: since it was charged, or handed back. The person keeps
/// it only while they are active, `hold` after their last input (or after
/// they took it by a click); then it is the boss's again, whatever the
/// session is doing, unless they pinned it. So a question or request the
/// session raises once they have stopped is the boss's to answer, and one
/// raised while they are still typing waits for them.
pub fn wheel(
    c: &Charge,
    last_person_input: Option<DateTime<Utc>>,
    hold: chrono::Duration,
    now: DateTime<Utc>,
) -> Wheel {
    let taken = match c.wheel {
        Wheel::PersonPinned => return Wheel::PersonPinned,
        Wheel::Person => Some(c.wheel_since.unwrap_or(c.since)),
        Wheel::Boss => None,
    };
    let boss_since = match c.wheel {
        Wheel::Boss => c.wheel_since.unwrap_or(c.since),
        Wheel::Person | Wheel::PersonPinned => c.since,
    };
    let input = last_person_input.filter(|t| *t > boss_since);
    match taken.max(input) {
        Some(last) if now - last < hold => Wheel::Person,
        _ => Wheel::Boss,
    }
}

// ── Where a boss acts fully ─────────────────────────────────────────────

/// Where a boss acting fully may ship, as its `[oversight.full]` says:
/// the repositories named, and the workflows it may run there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FullGrant {
    pub repos: Vec<String>,
    /// Workflow file names, matched exactly (`deploy.yml`).
    pub workflows: Vec<String>,
    /// `[oversight.full] anywhere`: no list limits it. It acts fully in
    /// every repository and runs any workflow; the repository checks of
    /// `repos` (its remotes, what `gh` resolves) are skipped.
    pub anywhere: bool,
    /// The boss is unbound ([`workforce_common::oversight::Oversight::unbound`]):
    /// it allows whatever its sessions ask, but for its own settings.
    pub unbound: bool,
}

/// Where a boss served from a grant (`<hub>/<agent>`) acts fully now, as
/// the hub judges it each time it is asked, from that boss's grant here as
/// it is then: none when it doesn't. Never kept, so a grant turned down
/// holds at the next thing asked.
pub trait ServedFull: Send + Sync {
    fn full_of(&self, boss: &str) -> Option<FullGrant>;
    /// Whether a live grant serving `boss` holds session `session` now.
    fn holds(&self, boss: &str, session: uuid::Uuid) -> bool;
}

/// A repository as a remote's address names it: its host, when said, and
/// its `owner/name`, lowercased, without `.git`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoName {
    pub host: Option<String>,
    pub path: String,
}

/// `text` (`git@github.com:acme/acme-web.git`, `https://github.com/acme/
/// acme-web`, `ssh://git@host:22/acme/acme-web.git`, `acme/acme-web`) as
/// the repository it names; `None` when it names none.
pub fn repo_name(text: &str) -> Option<RepoName> {
    let t = text.trim();
    if t.is_empty() || t.starts_with('/') || t.starts_with('~') || t.starts_with('.') {
        return None;
    }
    let (host, path) = if let Some((_, rest)) = t.split_once("://") {
        // scheme://[user@]host[:port]/path
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        (Some(host), path)
    } else if let Some((left, path)) = t.split_once(':') {
        // [user@]host:path
        (Some(left.rsplit('@').next()?), path)
    } else {
        let parts: Vec<&str> = t.split('/').collect();
        match parts.as_slice() {
            [owner, name] => (None, &t[..owner.len() + 1 + name.len()]),
            [host, _, _] if host.contains('.') => (Some(*host), &t[host.len() + 1..]),
            _ => return None,
        }
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).to_lowercase();
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 2
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return None;
    }
    Some(RepoName {
        host: host.filter(|h| !h.is_empty()).map(str::to_lowercase),
        path,
    })
}

/// Whether the repository a session works in is one of `named` (as
/// `[oversight.full] repos` writes them): by its `origin` remote's address
/// (`owner/name`, or the whole address), or by a local checkout's path,
/// which must be its main checkout or the worktree itself (`checkouts`,
/// real). A path named is read as `real` resolves it, `~` the home.
pub fn repo_named(
    origin: Option<&str>,
    checkouts: &[PathBuf],
    named: &[String],
    home: &Path,
    real: &dyn Fn(&Path) -> Option<PathBuf>,
) -> bool {
    let origin = origin.and_then(repo_name);
    let checkouts: Vec<PathBuf> = checkouts.iter().map(|c| boss::fold(c)).collect();
    named.iter().any(|n| {
        let n = n.trim();
        let path = if n == "~" {
            Some(home.to_path_buf())
        } else if let Some(rest) = n.strip_prefix("~/") {
            Some(home.join(rest))
        } else if n.starts_with('/') {
            Some(PathBuf::from(n))
        } else {
            None
        };
        if let Some(p) = path {
            return real(&p).is_some_and(|p| checkouts.contains(&boss::fold(&p)));
        }
        match (repo_name(n), &origin) {
            (Some(want), Some(have)) => same_repo(&want, have),
            _ => false,
        }
    })
}

/// The host a name without one means: GitHub's.
pub const DEFAULT_HOST: &str = "github.com";

/// Whether `want`, as the person named it, is the repository `have`: the
/// same `owner/name`, on the host named, `github.com` when none is.
fn same_repo(want: &RepoName, have: &RepoName) -> bool {
    let host = |r: &RepoName| r.host.clone().unwrap_or_else(|| DEFAULT_HOST.into());
    want.path == have.path && host(want) == host(have)
}

/// Whether every one of a repository's remote addresses (`remotes`, fetch
/// and push, every remote) is the same repository as its `origin`: so
/// whichever one `gh` resolves the repository to (its `gh-resolved`
/// default, then upstream, github, origin), it is that one. None at all,
/// or one that names no repository, isn't.
pub fn remotes_one_repo(origin: Option<&str>, remotes: &[String]) -> bool {
    let Some(origin) = origin.and_then(repo_name) else {
        return false;
    };
    !remotes.is_empty()
        && remotes
            .iter()
            .all(|r| repo_name(r).is_some_and(|r| same_repo(&r, &origin)))
}

/// Whether what `gh repo set-default` wrote (`remote.<name>.gh-resolved`:
/// `base`, that remote, or an `owner/name`) is the repository at `origin`:
/// each value `base` (its remote is checked with the rest) or that one.
pub fn gh_resolved_named(origin: Option<&str>, resolved: &[String]) -> bool {
    let Some(origin) = origin.and_then(repo_name) else {
        return false;
    };
    resolved.iter().all(|v| {
        v == "base"
            || repo_name(v).is_some_and(|r| {
                // `owner/name` alone is on origin's host.
                let r = RepoName {
                    host: r.host.or_else(|| origin.host.clone()),
                    path: r.path,
                };
                same_repo(&r, &origin)
            })
    })
}

// ── What a boss answers for ─────────────────────────────────────────────

/// Where a boss's answers for its sessions are judged: the person's home,
/// scratch space a delete may clear, and what no boss answers a request to
/// touch (Desk's own data, every agent's settings with it, the keychains).
/// The hub that runs the boss says; until it has, no boss answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bounds {
    pub home: PathBuf,
    pub scratch: Vec<PathBuf>,
    pub withheld: Vec<PathBuf>,
}

/// What a session's permission request does, as a boss's answer reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// A command, as the shell runs it.
    Command(String),
    /// Reads these paths (none: only the own folder).
    Read(Vec<PathBuf>),
    /// Fetches a web address; whether the address carries data (a query
    /// or a fragment).
    Web { carries: bool },
    /// Writes or edits these files.
    Change(Vec<PathBuf>),
    /// Works only inside the session: its to-do list, a helper of its own
    /// whose calls ask for themselves, a web search.
    Inside,
    /// One of the person's MCP servers' tools.
    Mcp {
        server: String,
        tool: String,
        /// The engine says it only reads.
        read_only: bool,
    },
    /// Something Desk can't tell: what it is, in the person's words.
    Unknown(String),
}

/// A session's permission request, with what it is judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The session's own folder: its cwd, or the worktree holding it.
    pub own: PathBuf,
    /// What the person calls the folder ("acme-web").
    pub folder: String,
    /// Where the request runs; `None`: not known (the session moved and
    /// its engine didn't say where to).
    pub cwd: Option<PathBuf>,
    /// The branch the folder is on, read from the repository.
    pub branch: Option<String>,
    /// The repository's default branch, when it says (`origin/HEAD`).
    pub default_branch: Option<String>,
    /// The boss acts fully in this repository: what ships is its to allow.
    pub ships: bool,
    /// Every remote of the repository is the one named: `gh` ships too.
    pub ships_gh: bool,
    /// The repository says `push.followTags`.
    pub follow_tags: bool,
    /// The repository has git hooks.
    pub hooks: bool,
    /// The workflows the boss may run where it acts fully, by file name.
    pub workflows: Vec<String>,
    /// Any workflow is the boss's to run where it acts fully (anywhere).
    pub any_workflow: bool,
    /// `HEAD` is the commit `origin`'s default branch is at.
    pub at_tip: bool,
    /// `HEAD` has commits `origin`'s default branch hasn't, or Desk can't
    /// tell.
    pub unpushed: bool,
    /// The repository's branch names, local and `origin`'s.
    pub branches: Vec<String>,
    /// The tags at `origin`'s default branch's commit.
    pub tip_tags: Vec<String>,
    pub what: What,
}

/// Why a boss doesn't answer a request itself, as the person reads it:
/// "deletes outside acme-web", "can't be undone".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Irreversible(pub String);

/// `raw` as a path: its quotes and escaped spaces taken off, `~` and
/// `$HOME` as the home, joined to `cwd` and read lexically. `None` when
/// it is relative and where it is read from isn't known.
fn path_of(raw: &str, cwd: Option<&Path>, home: &Path) -> Option<PathBuf> {
    let unquoted = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .replace("\\ ", " ");
    let text = unquoted
        .strip_prefix("${HOME}")
        .or_else(|| unquoted.strip_prefix("$HOME"))
        .map_or(unquoted.clone(), |rest| format!("~{rest}"));
    let joined = if text == "~" {
        home.to_path_buf()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home.join(rest)
    } else if text.starts_with('~') || text.contains('$') {
        return None;
    } else if Path::new(&text).is_absolute() {
        PathBuf::from(&text)
    } else {
        cwd?.join(&text)
    };
    // `.` and `..` stay as written: what they mean after a symlink is the
    // filesystem's to say.
    Some(joined)
}

/// What a pattern reads under: the part before its first wildcard.
fn under(pattern: &str) -> &str {
    let at = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    &pattern[..at]
}

/// The folder a session works in: its cwd, or the worktree under
/// `.worktrees/` that holds it.
pub(crate) fn own_folder(cwd: &Path) -> PathBuf {
    let parts: Vec<Component<'_>> = cwd.components().collect();
    match parts
        .iter()
        .position(|c| c.as_os_str() == ".worktrees")
        .filter(|at| at + 1 < parts.len())
    {
        Some(at) => parts[..at + 2].iter().collect(),
        None => cwd.to_path_buf(),
    }
}

/// Session `s`'s request `pending` as a boss's answer reads it, for its
/// `engine`. Commands (Claude's `Bash`, Codex's command) are read whole as
/// they run; file changes by every path they name; MCP tools by their
/// server and name. A request Desk wasn't sent in full is unknown. When
/// the session `moved` (its commands changed directory) and its engine
/// didn't say where the request runs, where is unknown. The repository's
/// branches are the caller's to read.
pub fn facts_of(
    engine: Engine,
    pending: &PendingApproval,
    s: &SessionView,
    home: &Path,
    moved: bool,
) -> Facts {
    let session_cwd = PathBuf::from(&s.cwd);
    let own = own_folder(&session_cwd);
    let folder = own
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| s.cwd.clone());
    let said = pending
        .request
        .as_ref()
        .and_then(|r| r.cwd.as_deref())
        .map(PathBuf::from)
        .filter(|c| c.is_absolute());
    let cwd = match (said, moved) {
        (Some(c), _) => Some(c),
        (None, false) => Some(session_cwd),
        (None, true) => None,
    };
    let what = match &pending.request {
        None => What::Unknown(format!("{} (Desk wasn't sent it in full)", pending.tool)),
        Some(r) => what_of(engine, r, cwd.as_deref(), home),
    };
    Facts {
        own,
        folder,
        cwd,
        branch: None,
        default_branch: None,
        ships: false,
        ships_gh: false,
        follow_tags: false,
        hooks: false,
        workflows: Vec::new(),
        any_workflow: false,
        at_tip: false,
        unpushed: true,
        branches: Vec::new(),
        tip_tags: Vec::new(),
        what,
    }
}

fn what_of(engine: Engine, r: &Request, cwd: Option<&Path>, home: &Path) -> What {
    let field = |k: &str| r.input.get(k).and_then(Value::as_str);
    // Every path named, or unknown when one can't be told.
    let paths = |raws: Vec<&str>| -> Option<Vec<PathBuf>> {
        raws.into_iter().map(|p| path_of(p, cwd, home)).collect()
    };
    let unknown = || What::Unknown("a path Desk can't tell".into());
    if let Some(rest) = r.tool.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
        return What::Mcp {
            server: server.to_string(),
            tool: tool.to_string(),
            read_only: r.read_only,
        };
    }
    match (engine, r.tool.as_str()) {
        (_, "Bash") => match field("command") {
            Some(command) => What::Command(command.to_string()),
            None => What::Unknown("a command Desk wasn't sent".into()),
        },
        (_, "Read" | "NotebookRead" | "LS" | "Grep" | "Glob") => {
            let mut raws: Vec<&str> = ["file_path", "notebook_path", "path"]
                .iter()
                .filter_map(|k| field(k))
                .collect();
            // What a pattern reads under, when it names a place.
            for k in ["pattern", "glob"] {
                if let Some(p) = field(k).filter(|_| r.tool != "Grep" || k == "glob") {
                    let at = under(p);
                    if at.contains('/') || at.starts_with('~') || at.starts_with('$') {
                        raws.push(at);
                    }
                }
            }
            paths(raws).map_or_else(unknown, What::Read)
        }
        (_, "WebFetch") => What::Web {
            carries: field("url").is_none_or(|u| u.contains('?') || u.contains('#')),
        },
        (_, "WebSearch" | "TodoWrite" | "Task" | "Agent") => What::Inside,
        // Codex names every file a change touches; a grant of a folder for
        // the rest of the session is the person's.
        (Engine::Codex, "Edit") => {
            if r.input.get("grantRoot").is_some_and(|g| !g.is_null()) {
                return What::Unknown(
                    "a grant to write in a folder for the rest of the session".into(),
                );
            }
            let raws: Vec<&str> = r
                .input
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            paths(raws).map_or_else(unknown, What::Change)
        }
        // Devin's patch names every file it touches ([`crate::devin`]
        // read them off it, and gave none it couldn't be sure of).
        (Engine::Devin, "Edit") if r.input.get("paths").is_some() => {
            let raws: Vec<&str> = r
                .input
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            paths(raws).map_or_else(unknown, What::Change)
        }
        (_, "Edit" | "MultiEdit" | "Write" | "NotebookEdit") => {
            let raws = ["file_path", "notebook_path"]
                .iter()
                .filter_map(|k| field(k))
                .collect();
            paths(raws).map_or_else(unknown, What::Change)
        }
        (_, other) => What::Unknown(other.to_string()),
    }
}

/// Whether a boss may answer a request with `facts` itself, in `bounds`:
/// only what Desk reads in full and judges safe ([`boss::judge`] for
/// commands). `real` says where a path leads through existing symlinks.
pub fn answerable(
    facts: &Facts,
    bounds: &Bounds,
    real: &dyn Fn(&Path) -> Option<PathBuf>,
    tracked: &dyn Fn(&Path) -> bool,
) -> Result<(), Irreversible> {
    let no = |why: String| Err(Irreversible(why));
    if !facts.own.is_absolute() || facts.own.parent().is_none() {
        return no("Desk can't tell which folder this session works in".into());
    }
    let Some(own_real) = real(&facts.own) else {
        return no("Desk can't tell which folder this session works in".into());
    };
    // A session working in the home folder (or above it) has no folder of
    // its own to speak of: nothing counts as inside it.
    let broad = boss::fold(&bounds.home).starts_with(boss::fold(&own_real));
    let own = (!broad).then_some(own_real.as_path());
    let inside = |p: &Path| {
        own.is_some_and(|o| p.starts_with(o)) || bounds.scratch.iter().any(|t| p.starts_with(t))
    };
    match &facts.what {
        What::Command(command) => {
            let here = boss::Here {
                cwd: facts.cwd.clone(),
                own,
                folder: &facts.folder,
                home: &bounds.home,
                scratch: &bounds.scratch,
                withheld: &bounds.withheld,
                real,
                branch: facts.branch.as_deref(),
                default_branch: facts.default_branch.as_deref(),
                ships: facts.ships,
                ships_gh: facts.ships_gh,
                follow_tags: facts.follow_tags,
                hooks: facts.hooks,
                workflows: &facts.workflows,
                any_workflow: facts.any_workflow,
                at_tip: facts.at_tip,
                unpushed: facts.unpushed,
                branches: &facts.branches,
                tip_tags: &facts.tip_tags,
                tracked,
            };
            boss::judge(command, &here).map_err(Irreversible)
        }
        What::Read(paths) => {
            for p in paths {
                match real(p) {
                    Some(p) if boss::secret(&p, &bounds.home, &bounds.withheld) => {
                        return no(boss::SECRETS.into())
                    }
                    Some(_) => {}
                    None => return no("reads through a link to nothing".into()),
                }
            }
            // A file credentials are kept in, by its name, wherever it is.
            match paths.iter().find_map(|p| crate::never::path(p)) {
                Some(n) => no(n.why().into()),
                None => Ok(()),
            }
        }
        What::Web { carries: true } => no("Desk can't tell what this address sends".into()),
        What::Web { carries: false } | What::Inside => Ok(()),
        What::Change(paths) => {
            if paths.is_empty() {
                return no("Desk can't tell what it changes".into());
            }
            for p in paths {
                let Some(p) = real(p) else {
                    return no("changes a file through a link to nothing".into());
                };
                if boss::settings(&p, &bounds.home, &bounds.withheld) {
                    return no(boss::SETTINGS.into());
                }
                if !inside(&p) {
                    return no(format!("changes a file outside {}", facts.folder));
                }
            }
            // A file credentials are kept in, by its name.
            match paths.iter().find_map(|p| crate::never::path(p)) {
                Some(n) => no(n.why().into()),
                None => Ok(()),
            }
        }
        What::Mcp {
            server,
            tool,
            read_only,
        } => {
            let named = format!("{server} · {tool}");
            let dotted = format!("{}.{tool}", server.rsplit('_').next().unwrap_or(server));
            let sends = sends_to_people(&dotted)
                || ["post", "send", "reply", "invite", "share"]
                    .iter()
                    .any(|w| tool.contains(w));
            if sends {
                no(format!("sends to people ({named})"))
            } else if *read_only {
                Ok(())
            } else {
                no(format!("Desk can't tell what {named} does"))
            }
        }
        What::Unknown(what) => no(format!("Desk can't tell what {what} does")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Approvals, GroupRef, SessionRecord, View};
    use serde_json::json;

    fn session(engine: Engine) -> SessionView {
        let now = Utc::now();
        let r = SessionRecord {
            id: Uuid::new_v4(),
            engine,
            view: View::Terminal,
            engine_version: None,
            approve_for_me: false,
            fast: false,
            account: None,
            name: "acme-web".into(),
            goal: String::new(),
            cwd: "/work/acme-web".into(),
            branch: None,
            origin: Origin::Desk,
            state: SessionState::Idle,
            approvals: Approvals::Desk,
            cli_session_id: None,
            transcript: None,
            last_prompt: None,
            last_reply: None,
            external_pid: None,
            restore_offer: false,
            terminal_id: None,
            display_name: None,
            folder_chosen: false,
            asked: None,
            created_at: now,
            updated_at: now,
        };
        SessionView::new(&r, true, None, None)
    }

    #[test]
    fn only_a_session_with_a_line_from_desk_can_be_taken_on_and_why_not_is_said() {
        let none = HashSet::new();
        let main = Scope::Ungrouped;
        assert_eq!(chargeable(&session(Engine::Claude), &main, &none), Ok(()));
        assert_eq!(chargeable(&session(Engine::Codex), &main, &none), Ok(()));

        let mut external = session(Engine::Claude);
        external.origin = Origin::External;
        let mut ended = session(Engine::Codex);
        ended.state = SessionState::Closed;
        let mut old = session(Engine::Claude);
        old.caps =
            crate::engine::session_caps(Engine::Claude, View::Terminal, Some("2.1.0"), false);
        let custom = session(Engine::Custom);
        let mut atlas = session(Engine::Claude);
        atlas.group = Some(GroupRef {
            id: "g-atlas".into(),
            name: "Atlas".into(),
        });
        let bossed = HashSet::from(["g-atlas".to_string()]);

        let said = |s: &SessionView, bossed: &HashSet<String>| {
            chargeable(s, &main, bossed).unwrap_err().to_string()
        };
        assert_eq!(
            said(&external, &none),
            "It runs in another terminal. Continue it in Desk first."
        );
        assert_eq!(said(&ended, &none), "It has ended.");
        assert_eq!(
            said(&custom, &none),
            "A custom command has no line for a boss to talk on."
        );
        assert_eq!(
            said(&old, &none),
            "Update Claude Code to 2.1.287 or later to message this session."
        );
        assert_eq!(
            said(&atlas, &bossed),
            "It's in Atlas, which has a boss of its own."
        );
        assert_eq!(
            chargeable(&atlas, &main, &none),
            Ok(()),
            "a group with no boss is the main boss's"
        );
        let mia = Scope::Group("g-atlas".into());
        assert_eq!(chargeable(&atlas, &mia, &bossed), Ok(()), "its own boss's");
        assert_eq!(
            chargeable(&session(Engine::Claude), &mia, &bossed)
                .unwrap_err()
                .to_string(),
            "It isn't in the group you oversee."
        );
    }

    #[test]
    fn a_boss_that_takes_on_everything_never_takes_back_a_released_session() {
        let none = HashSet::new();
        let m = Scope::Ungrouped;
        let s = session(Engine::Claude);
        assert!(takes_automatically(true, &s, None, &m, &none));
        assert!(
            !takes_automatically(false, &s, None, &m, &none),
            "only handed"
        );
        let mut released = Charge::new(s.id, "sarah", How::Taken, None);
        released.released_at = Some(Utc::now());
        assert!(!takes_automatically(true, &s, Some(&released), &m, &none));
        let charged = Charge::new(s.id, "sarah", How::Handed, None);
        assert!(!takes_automatically(true, &s, Some(&charged), &m, &none));
        let mut external = session(Engine::Claude);
        external.origin = Origin::External;
        assert!(!takes_automatically(true, &external, None, &m, &none));
    }

    #[test]
    fn only_starting_and_working_sessions_hold_a_slot() {
        let states: Vec<SessionState> = vec![
            SessionState::Starting,
            SessionState::Working,
            SessionState::Blocked,
            SessionState::Asking,
            SessionState::Done,
            SessionState::Idle,
            SessionState::Stopped,
        ];
        let charges: Vec<Charge> = states
            .iter()
            .map(|_| Charge::new(Uuid::new_v4(), "sarah", How::Started, None))
            .collect();
        let by: HashMap<Uuid, SessionState> = charges
            .iter()
            .zip(&states)
            .map(|(c, s)| (c.session, *s))
            .collect();
        assert_eq!(busy(&charges, &by), 2);
        let mut released = charges.clone();
        released[0].released_at = Some(Utc::now());
        assert_eq!(busy(&released, &by), 1, "a released session holds none");
    }

    #[test]
    fn the_persons_input_holds_the_wheel_only_while_they_are_active() {
        let now = Utc::now();
        let secs = |s: i64| now - chrono::Duration::seconds(s);
        let ago = |m: i64| now - chrono::Duration::minutes(m);
        let mut c = Charge::new(Uuid::new_v4(), "sarah", How::Handed, None);
        c.since = ago(60);
        assert_eq!(wheel(&c, None, HOLD, now), Wheel::Boss);
        assert_eq!(
            wheel(&c, Some(ago(61)), HOLD, now),
            Wheel::Boss,
            "input before the charge doesn't count"
        );
        assert_eq!(
            wheel(&c, Some(secs(30)), HOLD, now),
            Wheel::Person,
            "30 seconds after their last keystroke they are still at it"
        );
        assert_eq!(
            wheel(&c, Some(ago(3)), HOLD, now),
            Wheel::Boss,
            "3 minutes after it, an ask is the boss's"
        );
        assert_eq!(
            wheel(&c, Some(ago(2)), HOLD, now),
            Wheel::Boss,
            "the hold is up at 2 minutes"
        );
        // A longer hold, as the boss's settings say.
        let five = chrono::Duration::minutes(5);
        assert_eq!(wheel(&c, Some(ago(3)), five, now), Wheel::Person);
        assert_eq!(wheel(&c, Some(ago(5)), five, now), Wheel::Boss);

        // Handed back: what they typed before doesn't take it again.
        c.wheel = Wheel::Boss;
        c.wheel_since = Some(secs(90));
        assert_eq!(wheel(&c, Some(secs(100)), HOLD, now), Wheel::Boss);
        assert_eq!(wheel(&c, Some(secs(10)), HOLD, now), Wheel::Person);

        // Taken by a click with no input since: it comes back after the hold.
        c.wheel = Wheel::Person;
        c.wheel_since = Some(ago(3));
        assert_eq!(wheel(&c, None, HOLD, now), Wheel::Boss);
        c.wheel_since = Some(secs(60));
        assert_eq!(wheel(&c, None, HOLD, now), Wheel::Person);

        // Pinned, it stays.
        c.wheel = Wheel::PersonPinned;
        c.wheel_since = Some(ago(600));
        assert_eq!(wheel(&c, None, HOLD, now), Wheel::PersonPinned);
    }

    #[test]
    fn a_repository_is_named_by_its_origin_or_its_checkout() {
        let gh = |p: &str| RepoName {
            host: Some("github.com".into()),
            path: p.into(),
        };
        for (text, want) in [
            (
                "git@github.com:acme/acme-web.git",
                Some(gh("acme/acme-web")),
            ),
            (
                "https://github.com/Acme/Acme-Web",
                Some(gh("acme/acme-web")),
            ),
            (
                "ssh://git@github.com:22/acme/acme-web.git",
                Some(gh("acme/acme-web")),
            ),
            ("github.com/acme/acme-web", Some(gh("acme/acme-web"))),
            (
                "acme/acme-web",
                Some(RepoName {
                    host: None,
                    path: "acme/acme-web".into(),
                }),
            ),
            ("acme-web", None),
            ("~/dev/acme-web", None),
            ("https://github.com/acme", None),
        ] {
            assert_eq!(repo_name(text), want, "{text}");
        }
        let home = Path::new("/Users/sam");
        let real = |p: &Path| Some(p.to_path_buf());
        let origin = Some("git@github.com:acme/acme-web.git");
        let main = [PathBuf::from("/Users/sam/dev/acme-web")];
        let named = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for yes in [
            named(&["acme/acme-web"]),
            named(&["globex/globex-api", "ACME/acme-web"]),
            named(&["https://github.com/acme/acme-web.git"]),
            named(&["~/dev/acme-web"]),
            named(&["/Users/sam/dev/acme-web"]),
        ] {
            assert!(repo_named(origin, &main, &yes, home, &real), "{yes:?}");
        }
        // A name with no host is GitHub's: a lookalike elsewhere isn't it.
        let gitlab = Some("git@gitlab.com:acme/acme-web.git");
        assert!(!repo_named(
            gitlab,
            &[],
            &named(&["acme/acme-web"]),
            home,
            &real
        ));
        assert!(repo_named(
            gitlab,
            &[],
            &named(&["gitlab.com/acme/acme-web"]),
            home,
            &real
        ));
        // gh acts on the repository only where every remote is that one: a
        // fork whose upstream is its parent isn't.
        let fork = Some("git@github.com:acme/acme-web.git");
        let urls = |u: &[&str]| u.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(remotes_one_repo(
            fork,
            &urls(&[
                "git@github.com:acme/acme-web.git",
                "https://github.com/acme/acme-web"
            ])
        ));
        assert!(!remotes_one_repo(
            fork,
            &urls(&[
                "git@github.com:acme/acme-web.git",
                "git@github.com:globex/acme-web.git"
            ])
        ));
        assert!(!remotes_one_repo(fork, &[]));
        // `gh repo set-default` pointed at the parent: not the one named.
        assert!(gh_resolved_named(fork, &urls(&["base"])));
        assert!(gh_resolved_named(fork, &urls(&["acme/acme-web"])));
        assert!(gh_resolved_named(fork, &[]));
        assert!(!gh_resolved_named(fork, &urls(&["globex/acme-web"])));
        assert!(!gh_resolved_named(
            fork,
            &urls(&["base", "globex/acme-web"])
        ));
        assert!(!remotes_one_repo(
            None,
            &urls(&["git@github.com:acme/acme-web.git"])
        ));
        for no in [
            named(&[]),
            named(&["globex/globex-api"]),
            named(&["acme/acme-web-fork"]),
            named(&["gitlab.com/acme/acme-web"]),
            named(&["~/dev"]),
            named(&["/Users/sam/dev/acme-web/src"]),
            named(&["acme-web"]),
        ] {
            assert!(!repo_named(origin, &main, &no, home, &real), "{no:?}");
        }
        assert!(
            !repo_named(None, &[], &named(&["acme/acme-web"]), home, &real),
            "no origin, no checkout: not named"
        );
    }

    #[test]
    fn the_wheel_reads_and_writes_as_its_words() {
        for w in [Wheel::Boss, Wheel::Person, Wheel::PersonPinned] {
            assert_eq!(Wheel::parse(w.as_str()).unwrap(), w);
            assert_eq!(serde_json::to_value(w).unwrap(), w.as_str());
        }
        assert_eq!(How::parse("taken").unwrap(), How::Taken);
    }

    fn bounds() -> Bounds {
        Bounds {
            home: "/Users/sam".into(),
            scratch: vec!["/tmp".into()],
            withheld: vec![
                "/Users/sam/Library/Application Support/Desk".into(),
                "/Users/sam/Library/Keychains".into(),
            ],
        }
    }

    fn asking(tool: &str, input: Value, cwd: Option<&str>) -> PendingApproval {
        PendingApproval {
            id: Uuid::new_v4(),
            tool: tool.into(),
            detail: String::new(),
            since: Utc::now(),
            answerable: true,
            choices: Vec::new(),
            why: None,
            options: None,
            request: Some(Request {
                tool: tool.into(),
                input,
                cwd: cwd.map(str::to_string),
                read_only: false,
            }),
            call: None,
            agent: None,
            cli_session: None,
        }
    }

    fn acme(cwd: &str) -> SessionView {
        let mut s = session(Engine::Claude);
        s.cwd = cwd.into();
        s
    }

    /// As the kernel resolves: the folder's `l` links into Desk's data,
    /// `m` to the next repository, and `out` to nothing.
    fn real(p: &Path) -> Option<PathBuf> {
        let exists = |p: &Path| {
            crate::computer::boss::tests::EXISTING
                .iter()
                .any(|e| Path::new(e).starts_with(p))
        };
        let mut out = PathBuf::from("/");
        for c in p.components() {
            match c {
                Component::ParentDir if !exists(&out) => return None,
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(n) => {
                    out.push(n);
                    if out == Path::new("/Users/sam/dev/acme-web/l") {
                        out = PathBuf::from("/Users/sam/Library/Application Support/Desk");
                    } else if out == Path::new("/Users/sam/dev/acme-web/m") {
                        out = PathBuf::from("/Users/sam/dev/acme-api");
                    } else if out == Path::new("/Users/sam/dev/acme-web/out") {
                        return None;
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }

    fn judged_moved(
        engine: Engine,
        p: &PendingApproval,
        s: &SessionView,
        moved: bool,
    ) -> Result<(), String> {
        let mut facts = facts_of(engine, p, s, Path::new("/Users/sam"), moved);
        facts.branch = Some("feat/date-picker".into());
        facts.default_branch = Some("main".into());
        answerable(&facts, &bounds(), &real, &|_| false).map_err(|e| e.to_string())
    }

    fn judged(engine: Engine, p: &PendingApproval, s: &SessionView) -> Result<(), String> {
        judged_moved(engine, p, s, false)
    }

    fn command_in(s: &SessionView, c: &str) -> Result<(), String> {
        judged(
            Engine::Claude,
            &asking("Bash", json!({ "command": c }), None),
            s,
        )
    }

    fn command(c: &str) -> Result<(), String> {
        command_in(&acme("/Users/sam/dev/acme-web"), c)
    }

    #[test]
    fn a_boss_answers_ordinary_work_and_hands_the_rest_to_the_person() {
        for ok in [
            "cargo test -p hub",
            "git commit -m 'Fix the date picker'",
            "git push origin feat/date-picker",
            "gh pr create --fill",
            "rm -rf node_modules",
        ] {
            assert_eq!(command(ok), Ok(()), "{ok}");
        }
        assert_eq!(command("git push origin main"), Err(boss::PUBLISHES.into()));
        assert_eq!(
            command("rm -rf ../acme-api"),
            Err("deletes outside acme-web".into())
        );
        // Connectors' sends, by AC3a's list and by name.
        let mcp = |tool: &str, read_only: bool| {
            let mut p = asking(tool, json!({}), None);
            p.request.as_mut().unwrap().read_only = read_only;
            judged(Engine::Claude, &p, &acme("/Users/sam/dev/acme-web"))
        };
        assert_eq!(
            mcp("mcp__slack__chat_post_message", false),
            Err("sends to people (slack · chat_post_message)".into())
        );
        assert_eq!(
            mcp("mcp__slack__chat_post_message", true),
            Err("sends to people (slack · chat_post_message)".into()),
            "a send is a send, whatever the engine says"
        );
        assert_eq!(
            mcp("mcp__email__send", false),
            Err("sends to people (email · send)".into())
        );
        assert_eq!(
            mcp("mcp__acme_tracker__update_issue", false),
            Err("Desk can't tell what acme_tracker · update_issue does".into())
        );
        assert_eq!(mcp("mcp__acme_tracker__get_issue", true), Ok(()));
    }

    #[test]
    fn edits_are_judged_where_they_lead_against_the_sessions_own_folder() {
        let s = acme("/Users/sam/dev/acme-web");
        let edit = |path: &str| {
            judged(
                Engine::Claude,
                &asking("Edit", json!({ "file_path": path }), None),
                &s,
            )
        };
        assert_eq!(edit("/Users/sam/dev/acme-web/src/date.ts"), Ok(()));
        assert_eq!(edit("src/date.ts"), Ok(()), "relative to its folder");
        assert_eq!(edit("CLAUDE.md"), Ok(()), "the project's own notes");
        assert_eq!(
            edit("/Users/sam/dev/acme-api/src/main.rs"),
            Err("changes a file outside acme-web".into())
        );
        assert_eq!(
            edit("../acme-api/x"),
            Err("changes a file outside acme-web".into())
        );
        for gate in [
            "l/hub/agents/desk/workforce.toml",
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".codex/config.toml",
            ".mcp.json",
            ".git/config",
            ".git/hooks/pre-push",
            ".envrc",
            "~/.zshrc",
            "$HOME/.claude.json",
            "/Users/sam/Library/Application\\ Support/Desk/x",
        ] {
            assert_eq!(edit(gate), Err(boss::SETTINGS.into()), "{gate}");
        }
        let write = asking("Write", json!({ "file_path": "/tmp/notes.md" }), None);
        assert_eq!(judged(Engine::Claude, &write, &s), Ok(()));

        // A worktree is its own folder; the repo it came from isn't.
        let wt = acme("/Users/sam/dev/acme-web/.worktrees/date-picker/src");
        let facts = facts_of(
            Engine::Claude,
            &asking("Read", json!({}), None),
            &wt,
            Path::new("/Users/sam"),
            false,
        );
        assert_eq!(
            facts.own,
            PathBuf::from("/Users/sam/dev/acme-web/.worktrees/date-picker")
        );
        assert_eq!(facts.folder, "date-picker");
        assert_eq!(
            command_in(&wt, "rm -rf /Users/sam/dev/acme-web/dist"),
            Err("deletes outside date-picker".into())
        );
        assert_eq!(command_in(&wt, "rm -rf ../dist"), Ok(()));

        // A session in the home folder has no folder of its own, however
        // its case is written.
        let shouting = acme("/users/SAM");
        assert_eq!(
            command_in(&shouting, "rm -rf ~/dev"),
            Err("deletes outside SAM".into())
        );
        let home = acme("/Users/sam");
        assert_eq!(command_in(&home, "cargo test"), Ok(()));
        assert_eq!(
            command_in(&home, "rm -rf ~/dev"),
            Err("deletes outside sam".into())
        );
        let notes = asking("Edit", json!({ "file_path": "/Users/sam/notes.md" }), None);
        assert_eq!(
            judged(Engine::Claude, &notes, &home),
            Err("changes a file outside sam".into())
        );
    }

    #[test]
    fn reads_go_anywhere_but_where_secrets_are_kept() {
        let s = acme("/Users/sam/dev/acme-web");
        let read =
            |tool: &str, input: Value| judged(Engine::Claude, &asking(tool, input, None), &s);
        for (tool, input) in [
            ("Read", json!({ "file_path": "/etc/hosts" })),
            ("Read", json!({ "file_path": "../acme-api/README.md" })),
            ("Grep", json!({ "pattern": "TODO", "path": "src" })),
            ("Glob", json!({ "pattern": "src/**/*.rs" })),
            (
                "WebFetch",
                json!({ "url": "https://docs.example.com/guide" }),
            ),
            ("WebSearch", json!({ "query": "date picker" })),
            ("TodoWrite", json!({})),
        ] {
            assert_eq!(read(tool, input.clone()), Ok(()), "{tool} {input}");
        }
        for (tool, input) in [
            ("Read", json!({ "file_path": "~/.ssh/id_ed25519" })),
            ("Read", json!({ "file_path": "$HOME/.aws/credentials" })),
            (
                "Read",
                json!({ "file_path": "\"/Users/sam/.claude.json\"" }),
            ),
            (
                "Read",
                json!({ "file_path": "/Users/sam/Library/Application\\ Support/Desk/vault.db" }),
            ),
            ("Read", json!({ "file_path": "l/vault.db" })),
            ("Read", json!({ "file_path": "~/.config/gh/hosts.yml" })),
            ("Read", json!({ "file_path": "~/.codex/auth.json" })),
            ("Glob", json!({ "pattern": "/Users/sam/.ssh/*" })),
            ("Grep", json!({ "pattern": "token", "path": "~/.aws" })),
            ("Grep", json!({ "pattern": "token", "glob": "~/.ssh/*" })),
            ("LS", json!({ "path": "~/Library/Keychains" })),
        ] {
            assert_eq!(
                read(tool, input.clone()),
                Err(boss::SECRETS.into()),
                "{tool} {input}"
            );
        }
        assert_eq!(
            read("WebFetch", json!({ "url": "https://x.example/?q=secret" })),
            Err("Desk can't tell what this address sends".into())
        );
        assert_eq!(
            read("WebFetch", json!({ "url": "https://x.example/#data" })),
            Err("Desk can't tell what this address sends".into())
        );
    }

    #[test]
    fn codex_is_judged_by_what_runs_every_file_it_changes_and_never_a_grant() {
        let s = acme("/Users/sam/dev/acme-web");
        let run = |c: &str| {
            asking(
                "Bash",
                json!({ "command": c }),
                Some("/Users/sam/dev/acme-web"),
            )
        };
        assert_eq!(
            judged(Engine::Codex, &run("/bin/zsh -lc 'cargo test'"), &s),
            Ok(())
        );
        assert_eq!(
            judged(Engine::Codex, &run("/bin/zsh -lc 'rm -rf ../acme-api'"), &s),
            Err("deletes outside acme-web".into())
        );
        assert_eq!(
            judged(
                Engine::Codex,
                &run("/bin/zsh -lc 'git push origin main'"),
                &s
            ),
            Err(boss::PUBLISHES.into())
        );
        assert_eq!(judged(Engine::Codex, &run("cargo test"), &s), Ok(()));
        let change = |input: Value| judged(Engine::Codex, &asking("Edit", input, None), &s);
        assert_eq!(change(json!({ "paths": ["src/a.ts", "src/b.ts"] })), Ok(()));
        assert_eq!(
            change(json!({ "paths": ["src/a.ts", "/Users/sam/.zshrc"] })),
            Err(boss::SETTINGS.into())
        );
        assert_eq!(
            change(json!({ "paths": ["src/a.ts", "/Users/sam/dev/acme-api/a.ts"] })),
            Err("changes a file outside acme-web".into()),
            "a move's destination is one of its paths"
        );
        assert_eq!(
            change(json!({ "paths": [] })),
            Err("Desk can't tell what it changes".into())
        );
        assert_eq!(
            change(json!({ "paths": ["src/a.ts"], "grantRoot": "/Users/sam/dev" })),
            Err("Desk can't tell what a grant to write in a folder for the rest of the session does".into())
        );
    }

    /// Devin's calls as [`crate::devin::normalize`] leaves them: its patch
    /// judged on every file it names, its edits on their file, and a tool
    /// Desk didn't read never one of Claude Code's.
    #[test]
    fn devins_calls_are_judged_on_what_desk_read_of_them() {
        let s = acme("/Users/sam/dev/acme-web");
        let call = |tool: &str, input: Value| {
            let mut p = json!({"hook_event_name": "PermissionRequest", "tool_name": tool,
                "tool_input": input});
            crate::devin::normalize(&mut p, None);
            let tool = p["tool_name"].as_str().unwrap().to_string();
            judged(
                Engine::Devin,
                &asking(&tool, p["tool_input"].clone(), None),
                &s,
            )
        };
        let patch = |files: &[&str]| {
            let body: String = files
                .iter()
                .map(|f| format!("*** Update File: {f}\n@@\n-a\n+b\n"))
                .collect();
            json!({ "patch": format!("*** Begin Patch\n{body}*** End Patch\n") })
        };
        assert_eq!(
            call("apply_patch", patch(&["src/a.ts", "src/b.ts"])),
            Ok(())
        );
        assert_eq!(
            call("apply_patch", patch(&["src/a.ts", "/Users/sam/.zshrc"])),
            Err(boss::SETTINGS.into())
        );
        assert_eq!(
            call(
                "apply_patch",
                patch(&["src/a.ts", "/Users/sam/dev/acme-api/a.ts"])
            ),
            Err("changes a file outside acme-web".into())
        );
        assert_eq!(
            call(
                "edit",
                json!({"file_path": "src/a.ts", "old_str": "a", "new_str": "b"})
            ),
            Ok(())
        );
        assert_eq!(call("exec", json!({"command": "cargo test"})), Ok(()));
        // What Desk couldn't read whole: the person's.
        assert!(call("apply_patch", json!({"patch": "--- a/x\n+++ b/x\n"})).is_err());
        assert!(call("exec", json!({"command": "ls", "workdir": "/"})).is_err());
        assert!(call("Task", json!({"prompt": "anything"})).is_err());
        assert!(call("read", json!({})).is_err());
    }

    #[test]
    fn what_desk_wasnt_sent_in_full_or_cant_place_goes_to_the_person() {
        let s = acme("/Users/sam/dev/acme-web");
        let mut clipped = asking("Bash", json!({}), None);
        clipped.tool = "Bash(ls…)".into();
        clipped.request = None;
        assert_eq!(
            judged(Engine::Claude, &clipped, &s),
            Err("Desk can't tell what Bash(ls…) (Desk wasn't sent it in full) does".into())
        );
        assert_eq!(
            judged(Engine::Claude, &asking("KillShell", json!({}), None), &s),
            Err("Desk can't tell what KillShell does".into())
        );
        let nowhere = acme("relative");
        assert!(judged(
            Engine::Claude,
            &asking("TodoWrite", json!({}), None),
            &nowhere
        )
        .is_err());
        // It changed directory and its engine didn't say where it is now:
        // a relative path can't be placed.
        let rm = asking("Bash", json!({ "command": "rm -rf build" }), None);
        assert_eq!(judged(Engine::Claude, &rm, &s), Ok(()));
        assert_eq!(
            judged_moved(Engine::Claude, &rm, &s, true),
            Err("deletes somewhere Desk can't tell".into())
        );
        let rm_at = asking(
            "Bash",
            json!({ "command": "rm -rf build" }),
            Some("/Users/sam/dev/acme-web/src"),
        );
        assert_eq!(
            judged_moved(Engine::Claude, &rm_at, &s, true),
            Ok(()),
            "it said where"
        );
        let edit = asking("Edit", json!({ "file_path": "src/a.ts" }), None);
        assert_eq!(
            judged_moved(Engine::Claude, &edit, &s, true),
            Err("Desk can't tell what a path Desk can't tell does".into())
        );
    }

    #[test]
    fn a_path_leads_where_its_symlinks_do_and_never_through_one_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let sibling = base.join("sibling");
        let own = base.join("own");
        std::fs::create_dir_all(sibling.join("deep")).unwrap();
        std::fs::create_dir_all(&own).unwrap();
        std::os::unix::fs::symlink(sibling.join("deep"), own.join("l")).unwrap();
        std::os::unix::fs::symlink(base.join("nowhere/x.plist"), own.join("out")).unwrap();
        let real = boss::resolve_real;
        assert_eq!(
            real(&own.join("l/new/file.txt")),
            Some(sibling.join("deep/new/file.txt"))
        );
        assert_eq!(
            real(&own.join("l/../x")),
            Some(sibling.join("x")),
            "`..` after a link is the parent of where it leads"
        );
        assert_eq!(
            real(&own.join("src/../new.rs")),
            None,
            "`..` after what isn't there yet"
        );
        assert_eq!(real(&own.join("out")), None, "a link to nothing");
        assert_eq!(real(&own.join("out/../y")), None);
        // Edits judged the same way.
        let s = acme("/Users/sam/dev/acme-web");
        let edit = |path: &str| {
            judged(
                Engine::Claude,
                &asking("Write", json!({ "file_path": path }), None),
                &s,
            )
        };
        assert_eq!(
            edit("m/../x"),
            Err("changes a file outside acme-web".into())
        );
        assert_eq!(
            edit("out"),
            Err("changes a file through a link to nothing".into())
        );
        for gate in [
            ".husky/pre-commit",
            ".cargo/config.toml",
            ".github/workflows/ci.yml",
            ".github/actions/a/action.yml",
            ".gitlab-ci.yml",
            ".circleci/config.yml",
            ".buildkite/pipeline.yml",
        ] {
            assert_eq!(edit(gate), Err(boss::SETTINGS.into()), "{gate}");
        }
        assert_eq!(edit(".github/CODEOWNERS"), Ok(()));
    }
}
