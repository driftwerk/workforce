//! The first-run gates a CLI shows before a session can work, settled for a
//! session Desk starts: on a machine (or a fresh Mac) Claude Code or Codex
//! may never have run, and the person would land on its setup screens.
//!
//! Claude Code (checked on 2.1.318, signed in by a token): its onboarding
//! (the theme picker, then a sign-in it doesn't need) shows until
//! `~/.claude.json` has `hasCompletedOnboarding`; then its folder-trust
//! dialog shows until `projects[<folder>].hasTrustDialogAccepted`, keyed by
//! the folder as the system resolves it, and inherited by every folder
//! below a trusted one. Nothing else stands in the way. Its own onboarding
//! also writes the theme picked and `lastOnboardingVersion`; a `theme`
//! equal to its default (`dark`) it drops.
//!
//! Codex (checked on 0.166): signed in, its TUI asks "Trust this folder?"
//! until `config.toml` has `[projects."<folder>"] trust_level = "trusted"`.
//!
//! What is settled ([`Settle`]):
//! - onboarding, for every session: it is no one's decision;
//! - the folder's trust only when the person chose that folder themselves
//!   in Desk (their consent, recorded when the session was made), never for
//!   `/`, the home folder or a folder above it (trust is inherited down the
//!   tree), and a `false` Claude Code keeps is its default, so it is set;
//! - for any other start (an agent's, a boss's, a board's), trust only for
//!   Desk's worktree of a repo already trusted, and never over a `false`;
//! - Codex's `untrusted`, the person's own answer, is never changed.
//!
//! Only what's missing is set, in place: the order and layout of the rest
//! is kept (Claude Code's file is edited by its keys' raw JSON; Codex's
//! with `toml_edit`). A change is made under the file's lock, read again
//! right before a temporary copy (checked to parse) replaces it, with the
//! file's permissions, and the folder is synced after. Claude Code's lock
//! is its own (`<file>.lock`, a folder its lockfile makes with `mkdir`):
//! when it can't be had, nothing is written and the CLI asks. Codex locks
//! nothing; its `<file>.lock` is only Desk's, and the read-again compare is
//! what keeps a save of Codex's. A file that is empty or doesn't parse, or
//! a config folder that isn't there, is left alone.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eyre::{Context, Result};
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::Value;

/// A CLI's config file, and whether its folder may be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub path: PathBuf,
    /// The folder is the CLI's default one, which it makes itself on its
    /// first run; one the environment names is never made.
    pub make_folder: bool,
}

/// Claude Code's config: in `CLAUDE_CONFIG_DIR` when the session's
/// environment names one, else in its `HOME`. None when it names neither.
pub fn claude_config(env: &BTreeMap<String, String>) -> Option<Config> {
    let set = |k: &str| env.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    set("CLAUDE_CONFIG_DIR")
        .or_else(|| set("HOME"))
        .map(|dir| Config {
            path: dir.join(".claude.json"),
            make_folder: false,
        })
}

/// Codex's config: in `CODEX_HOME`, else `~/.codex` (which may be made).
pub fn codex_config(env: &BTreeMap<String, String>) -> Option<Config> {
    let set = |k: &str| env.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    match set("CODEX_HOME") {
        Some(dir) => Some(Config {
            path: dir.join("config.toml"),
            make_folder: false,
        }),
        None => set("HOME").map(|h| Config {
            path: h.join(".codex").join("config.toml"),
            make_folder: true,
        }),
    }
}

/// The session's home folder, for never trusting it: its environment's
/// `HOME`, else the hub's own (`hub`), so a session without one still
/// doesn't trust the home it runs in.
pub fn home_of(env: &BTreeMap<String, String>, hub: Option<PathBuf>) -> Option<PathBuf> {
    env.get("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or(hub.filter(|h| !h.as_os_str().is_empty()))
}

/// The theme Claude Code is given when Desk completes its onboarding:
/// `auto`, which follows the terminal's background, Desk's or another's.
/// Not its default, so it is kept.
pub const CLAUDE_THEME: &str = "auto";

/// Where Claude Code's settings may set its theme, as it reads them: the
/// managed settings, the user's (`$CLAUDE_CONFIG_DIR` or `~/.claude`), the
/// folder's and the folder's local ones. `/theme` writes the user's.
fn claude_settings_files(env: &BTreeMap<String, String>, cwd: &Path) -> Vec<PathBuf> {
    let set = |k: &str| env.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let managed = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode/managed-settings.json"
    } else {
        "/etc/claude-code/managed-settings.json"
    };
    let user = set("CLAUDE_CONFIG_DIR").or_else(|| set("HOME").map(|h| h.join(".claude")));
    [
        Some(PathBuf::from(managed)),
        user.map(|d| d.join("settings.json")),
    ]
    .into_iter()
    .flatten()
    .chain([
        cwd.join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.local.json"),
    ])
    .collect()
}

/// The `theme` a JSON file sets, if it is one and sets one.
fn theme_in(path: &Path) -> Option<String> {
    let doc: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    doc.get("theme")?.as_str().map(str::to_string)
}

/// Whether a Claude Code session in `cwd` with `env` may follow the
/// terminal's light or dark (theme `auto`): when nothing the person set
/// says otherwise. Any of its settings files setting a theme is theirs; so
/// is a theme in its config (`.claude.json`, its older place), but for
/// `dark`, its default, which it never keeps there of its own accord, and
/// `auto`. When it may, the config is set to `auto` too ([`follow_terminal`]),
/// so Claude Code started by hand in a Desk terminal follows as well. Reads
/// and writes files: call off the async runtime.
pub fn claude_follows_terminal(env: &BTreeMap<String, String>, cwd: &Path) -> bool {
    if claude_settings_files(env, cwd)
        .iter()
        .any(|f| theme_in(f).is_some())
    {
        return false;
    }
    let config = claude_config(env);
    let legacy = config.as_ref().and_then(|c| theme_in(&c.path));
    if legacy
        .as_deref()
        .is_some_and(|t| t != "dark" && t != "auto")
    {
        return false;
    }
    if let Some(c) = &config {
        if let Err(e) = follow_terminal(c) {
            tracing::warn!(config = %c.path.display(), error = %e, "Claude Code's config wasn't set to follow the terminal");
        }
    }
    true
}

/// Sets an existing Claude Code config's theme to `auto` where it has
/// none, or `dark`, its default: under its lock, read again, replaced
/// whole. Whether the file changed.
pub fn follow_terminal(config: &Config) -> Result<bool> {
    update(config, |text| {
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return Ok(None);
        };
        let mut top: Ordered =
            serde_json::from_str(text).context("Claude Code's config isn't a JSON object")?;
        match top.value("theme") {
            None => {}
            Some(Value::String(t)) if t == "dark" => {}
            Some(_) => return Ok(None),
        }
        top.set("theme", &Value::String(CLAUDE_THEME.into()))?;
        let mut out = serde_json::to_string_pretty(&top)?;
        out.push('\n');
        Ok(Some(out))
    })
}

/// What a session's start may settle.
#[derive(Debug, Clone, Copy)]
pub struct Settle<'a> {
    /// The session's folder.
    pub folder: &'a Path,
    /// The session's home folder: it, and every folder above it, is never
    /// trusted by Desk.
    pub home: Option<&'a Path>,
    /// The person chose the folder (see
    /// [`crate::model::SessionRecord::folder_chosen`]).
    pub chosen: bool,
    /// The CLI's version, when known.
    pub version: Option<&'a str>,
}

/// Whether, and how, Desk may trust the session's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MayTrust {
    /// The person chose it: trust it.
    Chosen,
    /// Desk's worktree of this repo: trust it if the repo is already.
    WorktreeOf(String),
    No,
}

/// The folder's key (as the system resolves it), and what may be done.
fn trust_of(s: &Settle<'_>) -> (String, MayTrust) {
    let real = fs::canonicalize(s.folder).unwrap_or_else(|_| s.folder.to_path_buf());
    let key = real.to_string_lossy().into_owned();
    let home = s
        .home
        .map(|h| fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));
    // `/`, home and above: a folder trusted there trusts everything below.
    let too_wide = real.parent().is_none() || home.is_some_and(|h| h.starts_with(&real));
    let may = if too_wide {
        MayTrust::No
    } else if s.chosen {
        MayTrust::Chosen
    } else {
        match crate::worktree::project_of(&key) {
            repo if repo != key && is_worktree_of(&real, Path::new(repo)) => {
                MayTrust::WorktreeOf(repo.to_string())
            }
            _ => MayTrust::No,
        }
    };
    (key, may)
}

/// `folder` is a git worktree of `repo`, as git keeps one: its `.git` file
/// names a folder under the repo's `.git/worktrees`, which names it back.
/// Not only a folder where Desk would put one.
fn is_worktree_of(folder: &Path, repo: &Path) -> bool {
    let check = || -> Option<bool> {
        let dot_git = folder.join(".git");
        let text = fs::read_to_string(&dot_git).ok()?;
        let named = text.trim().strip_prefix("gitdir:")?.trim();
        let admin = fs::canonicalize(folder.join(named)).ok()?;
        let worktrees = fs::canonicalize(repo.join(".git").join("worktrees")).ok()?;
        if admin.parent()? != worktrees {
            return Some(false);
        }
        let back = fs::read_to_string(admin.join("gitdir")).ok()?;
        let back = admin.join(back.trim());
        Some(fs::canonicalize(back).ok()? == fs::canonicalize(&dot_git).ok()?)
    };
    check().unwrap_or(false)
}

/// Completes Claude Code's onboarding in `config`, and trusts the folder as
/// [`Settle`] allows. Whether the file changed.
pub fn ready_claude(config: &Config, s: &Settle<'_>) -> Result<bool> {
    let (key, may) = trust_of(s);
    update(config, |text| {
        let mut top = match text {
            None => Ordered::default(),
            Some(t) if t.trim().is_empty() => {
                eyre::bail!("Claude Code's config is empty; left alone")
            }
            Some(t) => {
                serde_json::from_str(t).context("Claude Code's config isn't a JSON object")?
            }
        };
        if !settle_claude(&mut top, &key, &may, s.version)? {
            return Ok(None);
        }
        let mut out = serde_json::to_string_pretty(&top)?;
        out.push('\n');
        Ok(Some(out))
    })
}

/// Trusts the folder for Codex in `config` as [`Settle`] allows.
pub fn trust_codex(config: &Config, s: &Settle<'_>) -> Result<bool> {
    let (key, may) = trust_of(s);
    if may == MayTrust::No {
        return Ok(false);
    }
    update(config, |text| {
        let mut doc: toml_edit::DocumentMut = text
            .unwrap_or_default()
            .parse()
            .context("Codex's config isn't TOML")?;
        if !settle_codex(&mut doc, &key, &may)? {
            return Ok(None);
        }
        Ok(Some(doc.to_string()))
    })
}

/// A JSON object with its keys in the order the file has them, each value
/// kept as its raw text until it is changed.
#[derive(Debug, Default)]
struct Ordered(Vec<(String, Node)>);

#[derive(Debug)]
enum Node {
    Raw(Box<RawValue>),
    Map(Ordered),
}

impl Ordered {
    fn get(&self, key: &str) -> Option<&Node> {
        // The last of a key said twice, as JSON readers (Claude Code's too)
        // take it.
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// `key`'s value read as JSON.
    fn value(&self, key: &str) -> Option<Value> {
        match self.get(key)? {
            Node::Raw(r) => serde_json::from_str(r.get()).ok(),
            Node::Map(_) => Some(Value::Object(Default::default())),
        }
    }

    fn set(&mut self, key: &str, value: &Value) -> Result<()> {
        let raw = RawValue::from_string(serde_json::to_string(value)?)?;
        match self.0.iter_mut().rev().find(|(k, _)| k == key) {
            Some((_, v)) => *v = Node::Raw(raw),
            None => self.0.push((key.to_string(), Node::Raw(raw))),
        }
        Ok(())
    }

    /// `key`'s object, opened up for changing (made when missing).
    fn map(&mut self, key: &str, what: &str) -> Result<&mut Ordered> {
        let at = match self.0.iter().rposition(|(k, _)| k == key) {
            Some(at) => at,
            None => {
                self.0
                    .push((key.to_string(), Node::Map(Ordered::default())));
                self.0.len() - 1
            }
        };
        let node = &mut self.0[at].1;
        if let Node::Raw(r) = node {
            let opened: Ordered = serde_json::from_str(r.get())
                .with_context(|| format!("Claude Code's config has {what} that isn't an object"))?;
            *node = Node::Map(opened);
        }
        match node {
            Node::Map(m) => Ok(m),
            Node::Raw(_) => unreachable!("opened above"),
        }
    }
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, Box<RawValue>>()? {
                    out.push((k, Node::Raw(v)));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl Serialize for Ordered {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            match v {
                Node::Raw(r) => m.serialize_entry(k, r)?,
                Node::Map(o) => m.serialize_entry(k, o)?,
            }
        }
        m.end()
    }
}

/// Sets what's missing of Claude Code's first run in `top`. Whether it
/// changed anything.
fn settle_claude(
    top: &mut Ordered,
    key: &str,
    may: &MayTrust,
    version: Option<&str>,
) -> Result<bool> {
    let mut changed = false;
    if top.value("hasCompletedOnboarding") != Some(Value::Bool(true)) {
        top.set("hasCompletedOnboarding", &Value::Bool(true))?;
        // As its own onboarding leaves it: the theme it would have asked
        // for, unless one is set, and the version it was completed on.
        if top.get("theme").is_none() {
            top.set("theme", &Value::String(CLAUDE_THEME.into()))?;
        }
        if let (None, Some(v)) = (top.get("lastOnboardingVersion"), version) {
            top.set("lastOnboardingVersion", &Value::String(v.into()))?;
        }
        changed = true;
    }
    let trusted = |top: &Ordered, folder: &str| -> Option<Value> {
        match top.get("projects")? {
            Node::Raw(r) => serde_json::from_str::<Value>(r.get())
                .ok()?
                .get(folder)?
                .get("hasTrustDialogAccepted")
                .cloned(),
            Node::Map(m) => match m.get(folder)? {
                Node::Raw(r) => serde_json::from_str::<Value>(r.get())
                    .ok()?
                    .get("hasTrustDialogAccepted")
                    .cloned(),
                Node::Map(p) => p.value("hasTrustDialogAccepted"),
            },
        }
    };
    let now = trusted(top, key);
    let trust = match may {
        MayTrust::No => false,
        _ if now == Some(Value::Bool(true)) => false,
        MayTrust::Chosen => true,
        // Never over a `false` the folder has, for a start the person
        // didn't choose.
        MayTrust::WorktreeOf(repo) => {
            now.is_none() && trusted(top, repo) == Some(Value::Bool(true))
        }
    };
    if trust {
        top.map("projects", "projects")?
            .map(key, "a project")?
            .set("hasTrustDialogAccepted", &Value::Bool(true))?;
        changed = true;
    }
    Ok(changed)
}

/// Trusts `key` in Codex's `doc` as `may` allows, leaving any trust level
/// set. Whether it changed anything.
fn settle_codex(doc: &mut toml_edit::DocumentMut, key: &str, may: &MayTrust) -> Result<bool> {
    let level = |doc: &toml_edit::DocumentMut, folder: &str| -> Option<String> {
        doc.get("projects")?
            .as_table_like()?
            .get(folder)?
            .as_table_like()?
            .get("trust_level")?
            .as_str()
            .map(str::to_string)
    };
    if level(doc, key).is_some() {
        return Ok(false);
    }
    let trust = match may {
        MayTrust::No => false,
        MayTrust::Chosen => true,
        MayTrust::WorktreeOf(repo) => level(doc, repo).as_deref() == Some("trusted"),
    };
    if !trust {
        return Ok(false);
    }
    let root = doc.as_table_mut();
    if !root.contains_key("projects") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        root.insert("projects", toml_edit::Item::Table(t));
    }
    match &mut root["projects"] {
        toml_edit::Item::Table(projects) => {
            if !projects.contains_key(key) {
                let mut t = toml_edit::Table::new();
                // Within dotted keys (`projects."/x".trust_level = ...`),
                // dotted too: a header there would define it twice.
                t.set_dotted(projects.is_dotted());
                projects.insert(key, toml_edit::Item::Table(t));
            }
        }
        toml_edit::Item::Value(toml_edit::Value::InlineTable(projects)) => {
            if !projects.contains_key(key) {
                projects.insert(key, toml_edit::InlineTable::new().into());
            }
        }
        _ => eyre::bail!("Codex's config has projects that aren't a table"),
    }
    root["projects"][key]
        .as_table_like_mut()
        .ok_or_else(|| eyre::eyre!("Codex's config has a project {key} that isn't a table"))?
        .insert("trust_level", toml_edit::value("trusted"));
    Ok(true)
}

/// How long a change waits for the file's lock before leaving the file
/// alone.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// A lock untouched for this long is left over: the CLI's lockfile renews
/// its own every 5 s and gives one up after 10 s.
const LOCK_STALE: Duration = Duration::from_secs(20);
/// How many times a change is made again when the file changed under it.
const TRIES: usize = 5;
/// One per config Desk changes, held while it changes it: its changes to
/// one file, from any thread, are made one after another. A queue has no
/// limit of its own: each in it still waits at most [`LOCK_WAIT`] for the
/// CLI, so N at once behind a CLI's lock wait up to N times that.
static UPDATING: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

/// The queue for Desk's own changes to `path`.
fn updating(path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    let mut all = UPDATING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::sync::Arc::clone(all.entry(path.to_path_buf()).or_default())
}

/// Changes the config by `change` (its text, none when there is no file,
/// to the new text, none for nothing to change): under its lock, read again
/// right before the new text (checked to parse) replaces it, through a
/// temporary file in its folder with its permissions. Whether it changed.
fn update(
    config: &Config,
    change: impl Fn(Option<&str>) -> Result<Option<String>>,
) -> Result<bool> {
    let path = &config.path;
    // A link (a dotfiles checkout) is kept: what it points at is changed.
    let target = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).with_context(|| format!("following {}", path.display()))?
        }
        _ => path.to_path_buf(),
    };
    let dir = target
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no folder", target.display()))?;
    if !dir.is_dir() {
        if !config.make_folder {
            // A folder the environment names that isn't there: nothing to
            // settle, every start, so nothing said.
            return Ok(false);
        }
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("making {}", dir.display()))?;
    }
    // Nothing missing: no lock, no write. A read caught mid-write (a CLI
    // that doesn't replace it whole) is looked at again under the lock.
    if let Ok(Ok(None)) = read(&target).map(|t| change(t.as_deref())) {
        return Ok(false);
    }
    // Desk's own changes queue here, one at a time, so the lock's wait is
    // only ever for the CLI: never one of Desk's timing out behind its own.
    // By the file's real path, however it is named.
    let queue = updating(&fs::canonicalize(&target).unwrap_or_else(|_| target.clone()));
    let _ours = queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(_lock) = Lock::take(path) else {
        eyre::bail!("{} stayed locked; left for its CLI to ask", path.display());
    };
    clear_leftovers(&target);
    for _ in 0..TRIES {
        let before = read(&target)?;
        let Some(text) = change(before.as_deref())? else {
            return Ok(false);
        };
        parses(&target, &text)?;
        let tmp = write_beside(&target, &text)?;
        if read(&target)? != before {
            let _ = fs::remove_file(&tmp);
            continue;
        }
        if let Err(e) = fs::rename(&tmp, &target) {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("replacing {}", target.display()));
        }
        if let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all()) {
            tracing::warn!(folder = %dir.display(), error = %e, "couldn't sync a CLI's config folder");
        }
        return Ok(true);
    }
    eyre::bail!("{} kept changing while Desk changed it", target.display())
}

/// `text` parses as what `target` holds: never replaced by what doesn't.
fn parses(target: &Path, text: &str) -> Result<()> {
    if target.extension().is_some_and(|e| e == "toml") {
        toml::from_str::<toml::Table>(text)
            .map(drop)
            .with_context(|| {
                format!(
                    "Desk's change to {} isn't TOML; not written",
                    target.display()
                )
            })
    } else {
        serde_json::from_str::<serde_json::Map<String, Value>>(text)
            .map(drop)
            .with_context(|| {
                format!(
                    "Desk's change to {} isn't JSON; not written",
                    target.display()
                )
            })
    }
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The prefix of Desk's temporary copies of `target`.
fn leftover_prefix(target: &Path) -> String {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{name}.desk-")
}

/// Removes temporary copies an earlier change left (it stopped before its
/// rename): under the lock, so none is another's in the making.
fn clear_leftovers(target: &Path) {
    let (Some(dir), prefix) = (target.parent(), leftover_prefix(target)) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with(&prefix) && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// `text` in a new file beside `target`, with its permissions (`0600` for
/// a new one: it may come to hold a sign-in), flushed to disk.
fn write_beside(target: &Path, text: &str) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let tmp = target.with_file_name(format!(
        "{}{}-{}",
        leftover_prefix(target),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mode = fs::metadata(target)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("making {}", tmp.display()))?;
    let wrote = f
        .write_all(text.as_bytes())
        .and_then(|()| f.set_permissions(fs::Permissions::from_mode(mode)))
        .and_then(|()| f.sync_all());
    if let Err(e) = wrote {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    Ok(tmp)
}

/// The lock on a file: `<file>.lock`, a folder made with `mkdir`, as
/// Claude Code's lockfile takes it. Desk's own while its folder is the one
/// Desk made.
struct Lock {
    path: PathBuf,
    made: Made,
}

/// Which folder it is: its inode, and when it was made (an inode can be
/// used again for the next folder made there).
type Made = (u64, u64, i64, i64);

fn made(m: &fs::Metadata) -> Made {
    (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
}

impl Lock {
    /// Takes the lock, waiting up to [`LOCK_WAIT`] and breaking one left
    /// over; none when it is held all that time or can't be made.
    fn take(file: &Path) -> Option<Self> {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        let until = Instant::now() + LOCK_WAIT;
        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let read = fs::symlink_metadata(&path);
                    return Self::made(path, read);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let held = fs::symlink_metadata(&path).ok();
                    let stale = held
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|at| SystemTime::now().duration_since(at).ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        // Only the one judged stale: one taken since stays.
                        let same = fs::symlink_metadata(&path).ok().map(|m| made(&m))
                            == held.map(|m| made(&m));
                        if same {
                            let _ = fs::remove_dir(&path);
                        }
                        continue;
                    }
                    if Instant::now() >= until {
                        tracing::warn!(lock = %path.display(), "a CLI's config stayed locked; Desk left it alone");
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    tracing::warn!(lock = %path.display(), error = %e, "couldn't lock a CLI's config; Desk left it alone");
                    return None;
                }
            }
        }
    }
}

impl Lock {
    /// The lock Desk just made at `path`, known by what `read` says of it;
    /// when that can't be read, it can't be known as Desk's later, so it is
    /// removed now rather than left held.
    fn made(path: PathBuf, read: std::io::Result<fs::Metadata>) -> Option<Self> {
        match read {
            Ok(m) => Some(Self {
                path,
                made: made(&m),
            }),
            Err(e) => {
                let _ = fs::remove_dir(&path);
                tracing::warn!(lock = %path.display(), error = %e, "couldn't read the lock Desk made; Desk left the config alone");
                None
            }
        }
    }
}

impl Drop for Lock {
    /// Lets go only of the folder Desk made: one a CLI took after judging
    /// Desk's stale (held past [`LOCK_STALE`], which a change never is) is
    /// its own. What's left is the moment between the look and the removal.
    fn drop(&mut self) {
        let ours = fs::symlink_metadata(&self.path).is_ok_and(|m| made(&m) == self.made);
        if ours {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn json_of(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn claude(path: &Path) -> Config {
        Config {
            path: path.to_path_buf(),
            make_folder: false,
        }
    }

    fn chosen<'a>(folder: &'a Path, home: &'a Path) -> Settle<'a> {
        Settle {
            folder,
            home: Some(home),
            chosen: true,
            version: Some("2.1.318"),
        }
    }

    fn real(p: &Path) -> String {
        fs::canonicalize(p).unwrap().to_string_lossy().into_owned()
    }

    /// A git worktree `<repo>/.worktrees/<name>` as Desk makes one (the
    /// repo made first when it isn't there).
    fn git_worktree(repo: &Path, name: &str) -> PathBuf {
        let git = |dir: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.email=d@e", "-c", "user.name=d"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "{}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        if !repo.join(".git").exists() {
            fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", "main"]);
            git(repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        }
        let wt = repo.join(".worktrees").join(name);
        git(
            repo,
            &["worktree", "add", "-q", "-b", name, &wt.to_string_lossy()],
        );
        wt
    }

    /// Takes `<file>.lock` as Claude Code's lockfile does (`mkdir`, retried
    /// while it is there), runs `f`, and lets go.
    fn as_the_cli<T>(file: &Path, f: impl FnOnce() -> T) -> T {
        let lock = PathBuf::from(format!("{}.lock", file.display()));
        while fs::create_dir(&lock).is_err() {
            std::thread::sleep(Duration::from_millis(2));
        }
        let out = f();
        fs::remove_dir(&lock).unwrap();
        out
    }

    #[test]
    fn configs_are_where_the_sessions_environment_says() {
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let at = |c: Option<Config>| c.map(|c| (c.path, c.make_folder));
        assert_eq!(
            at(claude_config(&env(&[("HOME", "/home/dev")]))),
            Some((PathBuf::from("/home/dev/.claude.json"), false))
        );
        assert_eq!(
            at(claude_config(&env(&[
                ("HOME", "/home/dev"),
                ("CLAUDE_CONFIG_DIR", "/cfg")
            ]))),
            Some((PathBuf::from("/cfg/.claude.json"), false))
        );
        assert_eq!(
            at(claude_config(&env(&[]))),
            None,
            "never the hub's own home"
        );
        assert_eq!(
            at(codex_config(&env(&[("HOME", "/home/dev")]))),
            Some((PathBuf::from("/home/dev/.codex/config.toml"), true))
        );
        assert_eq!(
            at(codex_config(&env(&[
                ("HOME", "/home/dev"),
                ("CODEX_HOME", "/cx")
            ]))),
            Some((PathBuf::from("/cx/config.toml"), false))
        );
        assert_eq!(at(codex_config(&env(&[]))), None);
    }

    #[test]
    fn claude_code_follows_the_terminal_unless_the_person_set_a_theme() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("repo");
        fs::create_dir_all(folder.join(".claude")).unwrap();
        let env = BTreeMap::from([(
            "HOME".to_string(),
            home.path().to_string_lossy().into_owned(),
        )]);
        let config = home.path().join(".claude.json");
        let theme = || theme_in(&config);

        // No config yet: it follows, and none is made.
        assert!(claude_follows_terminal(&env, &folder));
        assert!(!config.exists());
        // None set, or its default `dark`: it follows, and the config says so.
        for kept in [
            r#"{"numStartups": 3}"#,
            r#"{"theme": "dark", "numStartups": 3}"#,
        ] {
            fs::write(&config, kept).unwrap();
            assert!(claude_follows_terminal(&env, &folder));
            assert_eq!(theme().as_deref(), Some("auto"));
            let doc: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
            assert_eq!(doc["numStartups"], 3, "the rest is kept");
        }
        // One of its own in the config is theirs.
        fs::write(&config, r#"{"theme": "light-daltonized"}"#).unwrap();
        assert!(!claude_follows_terminal(&env, &folder));
        assert_eq!(theme().as_deref(), Some("light-daltonized"));
        // `/theme` writes the user's settings, which win over the config.
        fs::write(&config, r#"{"theme": "dark"}"#).unwrap();
        let settings = home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, r#"{"theme": "dark"}"#).unwrap();
        assert!(!claude_follows_terminal(&env, &folder));
        assert_eq!(theme().as_deref(), Some("dark"), "the config is left alone");
        fs::remove_file(&settings).unwrap();
        // So do the folder's settings, and its local ones.
        for name in ["settings.json", "settings.local.json"] {
            let f = folder.join(".claude").join(name);
            fs::write(&f, r#"{"theme": "light"}"#).unwrap();
            assert!(!claude_follows_terminal(&env, &folder), "{name}");
            fs::remove_file(&f).unwrap();
        }
        // CLAUDE_CONFIG_DIR moves both the settings and the config.
        let dir = home.path().join("cfg");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), r#"{"theme": "light"}"#).unwrap();
        let mut moved = env.clone();
        moved.insert(
            "CLAUDE_CONFIG_DIR".into(),
            dir.to_string_lossy().into_owned(),
        );
        assert!(!claude_follows_terminal(&moved, &folder));
    }

    #[test]
    fn without_a_home_of_its_own_a_session_has_the_hubs() {
        let hub = Some(PathBuf::from("/home/hub"));
        let env = |h: &str| BTreeMap::from([("HOME".to_string(), h.to_string())]);
        assert_eq!(
            home_of(&env("/home/dev"), hub.clone()),
            Some("/home/dev".into())
        );
        assert_eq!(home_of(&BTreeMap::new(), hub.clone()), hub);
        assert_eq!(home_of(&env(""), hub.clone()), hub);
        assert_eq!(home_of(&BTreeMap::new(), None), None);
    }

    #[test]
    fn a_fresh_home_is_onboarded_and_a_chosen_folder_trusted() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-api");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        assert!(ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap());
        let doc = json_of(&config);
        assert_eq!(doc["hasCompletedOnboarding"], json!(true));
        assert_eq!(doc["theme"], json!("auto"));
        assert_eq!(doc["lastOnboardingVersion"], json!("2.1.318"));
        assert_eq!(
            doc["projects"][real(&folder)]["hasTrustDialogAccepted"],
            json!(true)
        );
        let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "it may come to hold a sign-in");
        let ino = fs::metadata(&config).unwrap().ino();
        assert!(
            !ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap(),
            "nothing to change the second time"
        );
        assert_eq!(fs::metadata(&config).unwrap().ino(), ino, "not rewritten");
        assert!(
            !home.path().join(".claude.json.lock").exists(),
            "the lock is let go"
        );
    }

    #[test]
    fn what_the_person_has_is_kept_in_its_order() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-web");
        fs::create_dir(&folder).unwrap();
        let key = real(&folder);
        let config = home.path().join(".claude.json");
        // As Claude Code writes it: two-space JSON, its keys unsorted.
        let before = format!(
            "{{\n  \"userID\": \"u1\",\n  \"numStartups\": 7,\n  \"theme\": \"light-daltonized\",\n  \
             \"oauthAccount\": {{\n    \"emailAddress\": \"sam@example.com\"\n  }},\n  \
             \"projects\": {{\n    \"/elsewhere\": {{\n      \"hasTrustDialogAccepted\": false,\n      \
             \"allowedTools\": [\n        \"Bash\"\n      ]\n    }},\n    \"{key}\": {{\n      \
             \"hasTrustDialogAccepted\": false,\n      \"lastCost\": 1.5\n    }}\n  }},\n  \
             \"autoUpdates\": false\n}}\n"
        );
        fs::write(&config, &before).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap());
        let after = fs::read_to_string(&config).unwrap();
        let want = before
            .replace(
                &format!("\"{key}\": {{\n      \"hasTrustDialogAccepted\": false"),
                &format!("\"{key}\": {{\n      \"hasTrustDialogAccepted\": true"),
            )
            .replace(
                "  \"autoUpdates\": false\n}",
                "  \"autoUpdates\": false,\n  \"hasCompletedOnboarding\": true,\n  \
                 \"lastOnboardingVersion\": \"2.1.318\"\n}",
            );
        assert_eq!(
            after, want,
            "only what was missing changed, the rest as it was"
        );
        let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "its permissions are kept");
    }

    #[test]
    fn onboarding_done_its_theme_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude.json");
        fs::write(&config, r#"{"hasCompletedOnboarding":true}"#).unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap();
        let doc = json_of(&config);
        assert_eq!(
            doc.get("theme"),
            None,
            "Claude Code drops its default theme, so adding it would change the file every start"
        );
        assert_eq!(doc.get("lastOnboardingVersion"), None);
    }

    #[test]
    fn home_and_what_is_above_it_are_never_trusted() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home/dev");
        fs::create_dir_all(&home).unwrap();
        let config = home.join(".claude.json");
        for folder in [home.clone(), root.path().join("home"), PathBuf::from("/")] {
            ready_claude(&claude(&config), &chosen(&folder, &home)).unwrap();
            let doc = json_of(&config);
            assert_eq!(doc["hasCompletedOnboarding"], json!(true));
            assert_eq!(
                doc["projects"].get(real(&folder)),
                None,
                "{} would trust everything below it",
                folder.display()
            );
        }
        let codex = Config {
            path: home.join(".codex/config.toml"),
            make_folder: true,
        };
        assert!(!trust_codex(&codex, &chosen(&home, &home)).unwrap());
        assert!(!codex.path.exists());
    }

    #[test]
    fn a_start_the_person_didnt_choose_trusts_only_a_worktree_of_a_trusted_repo() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("acme-api");
        let wt = git_worktree(&repo, "login-race");
        let other = home.path().join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        // Where Desk puts one, but no worktree: never trusted as one.
        let shaped = repo.join(".worktrees/not-one");
        fs::create_dir_all(&shaped).unwrap();
        let config = home.path().join(".claude.json");
        let agent = |folder| Settle {
            folder,
            home: Some(home.path()),
            chosen: false,
            version: None,
        };
        ready_claude(&claude(&config), &agent(&other)).unwrap();
        ready_claude(&claude(&config), &agent(&wt)).unwrap();
        let doc = json_of(&config);
        assert_eq!(
            doc["hasCompletedOnboarding"],
            json!(true),
            "onboarding for every start"
        );
        assert_eq!(
            doc.get("projects"),
            None,
            "nothing trusted for an agent's start: {doc}"
        );

        let doc = json!({
            "hasCompletedOnboarding": true,
            "projects": { real(&repo): { "hasTrustDialogAccepted": true } }
        });
        fs::write(&config, doc.to_string()).unwrap();
        assert!(ready_claude(&claude(&config), &agent(&wt)).unwrap());
        assert_eq!(
            json_of(&config)["projects"][real(&wt)]["hasTrustDialogAccepted"],
            json!(true),
            "Desk's worktree of a trusted repo"
        );
        assert!(!ready_claude(&claude(&config), &agent(&shaped)).unwrap());
        assert_eq!(json_of(&config)["projects"].get(real(&shaped)), None);
        // Another repo's worktree, put in this repo's folder.
        let elsewhere = git_worktree(&home.path().join("acme-web"), "x");
        let moved = repo.join(".worktrees/x");
        fs::rename(&elsewhere, &moved).unwrap();
        assert!(!ready_claude(&claude(&config), &agent(&moved)).unwrap());

        // A `false` stays for a start the person didn't choose.
        let wt2 = git_worktree(&repo, "other");
        let mut doc = json_of(&config);
        doc["projects"][real(&wt2)] = json!({ "hasTrustDialogAccepted": false });
        fs::write(&config, doc.to_string()).unwrap();
        assert!(!ready_claude(&claude(&config), &agent(&wt2)).unwrap());
        assert_eq!(
            json_of(&config)["projects"][real(&wt2)]["hasTrustDialogAccepted"],
            json!(false)
        );
    }

    #[test]
    fn a_folder_through_a_link_is_trusted_only_where_it_resolves() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("real");
        fs::create_dir(&target).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let config = home.path().join(".claude.json");
        ready_claude(&claude(&config), &chosen(&link, home.path())).unwrap();
        let projects = json_of(&config)["projects"].clone();
        assert_eq!(
            projects,
            json!({ real(&target): { "hasTrustDialogAccepted": true } })
        );
    }

    #[test]
    fn a_linked_config_stays_a_link() {
        let home = tempfile::tempdir().unwrap();
        let kept = home.path().join("dotfiles.json");
        fs::write(&kept, "{}").unwrap();
        let config = home.path().join(".claude.json");
        std::os::unix::fs::symlink(&kept, &config).unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap();
        assert!(fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(json_of(&kept)["hasCompletedOnboarding"], json!(true));
    }

    #[test]
    fn an_empty_or_unreadable_config_or_a_missing_folder_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        for text in ["", "  \n", r#"{"userID": "u1", "#, "[1, 2]"] {
            fs::write(&config, text).unwrap();
            assert!(
                ready_claude(&claude(&config), &chosen(&folder, home.path())).is_err(),
                "{text:?}"
            );
            assert_eq!(fs::read_to_string(&config).unwrap(), text);
        }
        let codex = Config {
            path: home.path().join("config.toml"),
            make_folder: false,
        };
        fs::write(&codex.path, "model = [").unwrap();
        assert!(trust_codex(&codex, &chosen(&folder, home.path())).is_err());
        assert_eq!(fs::read_to_string(&codex.path).unwrap(), "model = [");

        // A CLAUDE_CONFIG_DIR that isn't there is never made, and quietly.
        let missing = home.path().join("cfg/.claude.json");
        assert!(!ready_claude(&claude(&missing), &chosen(&folder, home.path())).unwrap());
        assert!(!follow_terminal(&claude(&missing)).unwrap());
        assert!(!home.path().join("cfg").exists());
        let missing = Config {
            path: home.path().join("cx/config.toml"),
            make_folder: false,
        };
        assert!(!trust_codex(&missing, &chosen(&folder, home.path())).unwrap());
        assert!(!home.path().join("cx").exists());
        // Nor is a config that isn't there yet.
        let absent = home.path().join("none.json");
        assert!(!follow_terminal(&claude(&absent)).unwrap());
        assert!(!absent.exists());
    }

    #[test]
    fn a_held_lock_is_waited_for_and_a_left_over_one_broken() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        let lock = home.path().join(".claude.json.lock");
        fs::create_dir(&lock).unwrap();
        // The CLI saves and lets go while Desk waits.
        let saver = {
            let (config, lock) = (config.clone(), lock.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                fs::write(&config, r#"{"numStartups": 3}"#).unwrap();
                fs::remove_dir(&lock).unwrap();
            })
        };
        let started = Instant::now();
        ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap();
        saver.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250), "it waited");
        let doc = json_of(&config);
        assert_eq!(doc["numStartups"], json!(3), "the CLI's save is kept");
        assert_eq!(doc["hasCompletedOnboarding"], json!(true));

        // One left over from a CLI that died holding it.
        fs::create_dir(&lock).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        fs::File::open(&lock).unwrap().set_modified(old).unwrap();
        let other = home.path().join("q");
        fs::create_dir(&other).unwrap();
        let started = Instant::now();
        assert!(ready_claude(&claude(&config), &chosen(&other, home.path())).unwrap());
        assert!(
            started.elapsed() < LOCK_WAIT,
            "a left-over lock isn't waited out"
        );
        assert!(!lock.exists());
    }

    #[test]
    fn a_lock_held_throughout_leaves_the_file_alone() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        fs::write(&config, r#"{"userID":"u1"}"#).unwrap();
        let lock = home.path().join(".claude.json.lock");
        fs::create_dir(&lock).unwrap();
        // Kept fresh, as the CLI's lockfile renews it.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let renew = {
            let (lock, stop) = (lock.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = fs::File::open(&lock).and_then(|f| f.set_modified(SystemTime::now()));
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
        };
        let got = ready_claude(&claude(&config), &chosen(&folder, home.path()));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        renew.join().unwrap();
        assert!(got.is_err(), "never written without the lock");
        assert_eq!(fs::read_to_string(&config).unwrap(), r#"{"userID":"u1"}"#);
        assert!(lock.exists(), "the CLI's lock is the CLI's");
    }

    #[test]
    fn a_lock_desk_no_longer_holds_isnt_removed() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".claude.json");
        let lock = home.path().join(".claude.json.lock");
        let held = Lock::take(&file).unwrap();
        // Judged stale and taken by the CLI meanwhile (the same inode, it
        // may be).
        fs::remove_dir(&lock).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::create_dir(&lock).unwrap();
        drop(held);
        assert!(lock.exists(), "the CLI's lock stays");
    }

    #[test]
    fn leftover_copies_are_cleared() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        let left = home.path().join(".claude.json.desk-1-abc");
        fs::write(&left, "{").unwrap();
        let other = home.path().join(".claude.json.backup");
        fs::write(&other, "{}").unwrap();
        ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap();
        assert!(!left.exists());
        assert!(other.exists(), "only Desk's own copies");
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude.json");
        fs::write(&config, "{}").unwrap();
        // A stand-in CLI saving its counter under its lock, taken its way,
        // while Desk trusts folders.
        let cli = {
            let config = config.clone();
            std::thread::spawn(move || {
                for n in 1..=40 {
                    as_the_cli(&config, || {
                        let mut doc = json_of(&config);
                        doc["numStartups"] = json!(n);
                        fs::write(&config, doc.to_string()).unwrap();
                    });
                }
            })
        };
        let desks: Vec<_> = (0..4)
            .map(|i| {
                let (config, root) = (config.clone(), home.path().to_path_buf());
                std::thread::spawn(move || {
                    for j in 0..10 {
                        let folder = root.join(format!("f{i}-{j}"));
                        fs::create_dir(&folder).unwrap();
                        ready_claude(&claude(&config), &chosen(&folder, &root)).unwrap();
                    }
                })
            })
            .collect();
        cli.join().unwrap();
        for d in desks {
            d.join().unwrap();
        }
        let doc = json_of(&config);
        assert_eq!(
            doc["numStartups"],
            json!(40),
            "every save of the CLI's is kept"
        );
        assert_eq!(doc["hasCompletedOnboarding"], json!(true));
        for i in 0..4 {
            for j in 0..10 {
                let f = real(&home.path().join(format!("f{i}-{j}")));
                assert_eq!(
                    doc["projects"][&f]["hasTrustDialogAccepted"],
                    json!(true),
                    "{f} is trusted"
                );
            }
        }
    }

    #[test]
    fn codex_trusts_a_chosen_folder_and_keeps_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-api");
        fs::create_dir(&folder).unwrap();
        let key = real(&folder);
        let config = Config {
            path: home.path().join(".codex/config.toml"),
            make_folder: true,
        };
        assert!(
            trust_codex(&config, &chosen(&folder, home.path())).unwrap(),
            "made with its folder"
        );
        let doc: toml::Table = fs::read_to_string(&config.path).unwrap().parse().unwrap();
        assert_eq!(
            doc["projects"][&key]["trust_level"].as_str(),
            Some("trusted")
        );
        assert!(!trust_codex(&config, &chosen(&folder, home.path())).unwrap());

        let kept = "# my settings\nmodel = \"gpt-6\"\n\n[projects.\"/elsewhere\"]\ntrust_level = \"untrusted\"\n";
        fs::write(&config.path, kept).unwrap();
        trust_codex(&config, &chosen(&folder, home.path())).unwrap();
        let text = fs::read_to_string(&config.path).unwrap();
        assert!(
            text.starts_with(kept),
            "the person's file, comments too, is kept: {text}"
        );
        assert!(text.contains("trust_level = \"trusted\""), "{text}");

        let untrusted = home.path().join("no");
        fs::create_dir(&untrusted).unwrap();
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        doc["projects"][&real(&untrusted)]["trust_level"] = toml_edit::value("untrusted");
        fs::write(&config.path, doc.to_string()).unwrap();
        assert!(
            !trust_codex(&config, &chosen(&untrusted, home.path())).unwrap(),
            "a folder the person chose not to trust stays so"
        );
    }

    #[test]
    fn codex_trust_fits_how_the_file_writes_its_projects() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-api");
        fs::create_dir(&folder).unwrap();
        let key = real(&folder);
        let config = Config {
            path: home.path().join("config.toml"),
            make_folder: false,
        };
        for kept in [
            "projects.\"/a\".trust_level = \"trusted\"\n",
            "[projects]\n\"/a\".trust_level = \"trusted\"\n",
            "projects = { \"/a\" = { trust_level = \"trusted\" } }\n",
            "[projects]\n",
        ] {
            fs::write(&config.path, kept).unwrap();
            assert!(
                trust_codex(&config, &chosen(&folder, home.path())).unwrap(),
                "{kept}"
            );
            let text = fs::read_to_string(&config.path).unwrap();
            let doc: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
            assert_eq!(
                doc["projects"][&key]["trust_level"].as_str(),
                Some("trusted"),
                "{text}"
            );
            if kept.contains("/a") {
                assert_eq!(
                    doc["projects"]["/a"]["trust_level"].as_str(),
                    Some("trusted"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn codex_for_a_start_the_person_didnt_choose() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("acme-api");
        let wt = git_worktree(&repo, "x");
        let config = Config {
            path: home.path().join("config.toml"),
            make_folder: false,
        };
        let agent = |folder| Settle {
            folder,
            home: Some(home.path()),
            chosen: false,
            version: None,
        };
        assert!(!trust_codex(&config, &agent(&repo)).unwrap());
        assert!(
            !trust_codex(&config, &agent(&wt)).unwrap(),
            "its repo isn't trusted"
        );
        assert!(!config.path.exists());
        fs::write(
            &config.path,
            format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                real(&repo)
            ),
        )
        .unwrap();
        assert!(trust_codex(&config, &agent(&wt)).unwrap());
        let doc: toml::Table = fs::read_to_string(&config.path).unwrap().parse().unwrap();
        assert_eq!(
            doc["projects"][&real(&wt)]["trust_level"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn a_key_said_twice_is_read_as_its_last() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("p");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        let key = real(&folder);
        let text = format!(
            "{{\"hasCompletedOnboarding\": false, \"hasCompletedOnboarding\": true, \
             \"projects\": {{\"{key}\": {{\"hasTrustDialogAccepted\": false}}}}, \
             \"projects\": {{\"{key}\": {{\"hasTrustDialogAccepted\": true}}}}}}"
        );
        fs::write(&config, &text).unwrap();
        assert!(
            !ready_claude(&claude(&config), &chosen(&folder, home.path())).unwrap(),
            "settled already, by the last of each"
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), text);

        let text = r#"{"theme": "light", "hasCompletedOnboarding": true, "hasCompletedOnboarding": false}"#;
        fs::write(&config, text).unwrap();
        let other = home.path().join("q");
        fs::create_dir(&other).unwrap();
        let s = Settle {
            folder: &other,
            home: Some(home.path()),
            chosen: false,
            version: None,
        };
        assert!(ready_claude(&claude(&config), &s).unwrap());
        let after: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            after["hasCompletedOnboarding"],
            json!(true),
            "the last one is set"
        );
    }

    #[test]
    fn a_lock_whose_folder_cant_be_read_isnt_left_held() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join(".claude.json.lock");
        fs::create_dir(&lock).unwrap();
        let unread = Err(std::io::Error::other("can't be read"));
        assert!(Lock::made(lock.clone(), unread).is_none());
        assert!(!lock.exists(), "removed, not left held");
    }
}
