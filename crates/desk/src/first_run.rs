//! The first-run gates a CLI shows before a session can work, settled for a
//! session Desk starts: on a machine (or a fresh Mac) Claude Code or Codex
//! may never have run, and the person would land on its setup screens.
//!
//! Claude Code (checked on 2.1.318, signed in by a token): its onboarding
//! (the theme picker, then a sign-in it doesn't need) shows until
//! `~/.claude.json` has `hasCompletedOnboarding`; then its folder-trust
//! dialog shows until `projects[<folder>].hasTrustDialogAccepted`, keyed by
//! the folder as the system resolves it (a link's own path isn't looked up),
//! found in the folder or one above it. Nothing else stands in the way.
//! When it completes onboarding itself it also writes the theme picked and
//! `lastOnboardingVersion`; a `theme` equal to its default (`dark`) it drops.
//!
//! Codex (checked on 0.166): signed in, its TUI asks "Trust this folder?"
//! until `config.toml` has `[projects."<folder>"] trust_level = "trusted"`
//! (its own answer keys a worktree by its repo; the folder's own entry is
//! found too).
//!
//! Only what's missing is set; what the person chose stays. Choosing the
//! folder in Desk is their consent to trust it. Each file is changed under
//! Claude Code's own lock (`<file>.lock`, a folder made with `mkdir`, as
//! the CLI's lockfile takes it), re-read right before a temporary copy
//! replaces it, and written with its own permissions, so a CLI saving it
//! meanwhile loses nothing, and a file that doesn't parse is left alone.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eyre::{Context, Result};
use serde_json::{Map, Value};

/// Claude Code's config: in `CLAUDE_CONFIG_DIR` when the session's
/// environment names one, else in its `HOME`. None when it names neither:
/// the hub's own home is never assumed.
pub fn claude_config(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let set = |k: &str| env.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    set("CLAUDE_CONFIG_DIR")
        .or_else(|| set("HOME"))
        .map(|dir| dir.join(".claude.json"))
}

/// Codex's config: in `CODEX_HOME`, else `~/.codex`, by the session's
/// environment.
pub fn codex_config(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let set = |k: &str| env.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    set("CODEX_HOME")
        .or_else(|| set("HOME").map(|h| h.join(".codex")))
        .map(|dir| dir.join("config.toml"))
}

/// The theme Claude Code is given when Desk completes its onboarding.
pub const CLAUDE_THEME: &str = "dark";

/// Completes Claude Code's onboarding in `config` and trusts `folder`
/// (as given and as resolved). `version` is the CLI's, when known. Whether
/// the file changed.
pub fn ready_claude(config: &Path, folder: &Path, version: Option<&str>) -> Result<bool> {
    let folders = folder_keys(folder);
    update(config, |text| {
        let mut doc: Value = match text {
            None => Value::Object(Map::new()),
            Some(t) if t.trim().is_empty() => Value::Object(Map::new()),
            Some(t) => serde_json::from_str(t).context("Claude Code's config isn't JSON")?,
        };
        if !settle_claude(&mut doc, &folders, version)? {
            return Ok(None);
        }
        let mut out = serde_json::to_string_pretty(&doc)?;
        out.push('\n');
        Ok(Some(out))
    })
}

/// Trusts `folder` (as given and as resolved) for Codex in `config`.
pub fn trust_codex(config: &Path, folder: &Path) -> Result<bool> {
    let folders = folder_keys(folder);
    update(config, |text| {
        let mut doc: toml_edit::DocumentMut = text
            .unwrap_or_default()
            .parse()
            .context("Codex's config isn't TOML")?;
        if !settle_codex(&mut doc, &folders)? {
            return Ok(None);
        }
        Ok(Some(doc.to_string()))
    })
}

/// `folder` as given, and as the system resolves it when that differs:
/// Claude Code looks up the resolved one.
fn folder_keys(folder: &Path) -> Vec<String> {
    let mut keys = vec![folder.to_string_lossy().into_owned()];
    if let Ok(real) = fs::canonicalize(folder) {
        let real = real.to_string_lossy().into_owned();
        if !keys.contains(&real) {
            keys.push(real);
        }
    }
    keys
}

/// Sets what's missing of Claude Code's first run in `doc`. Whether it
/// changed anything.
fn settle_claude(doc: &mut Value, folders: &[String], version: Option<&str>) -> Result<bool> {
    let Value::Object(top) = doc else {
        eyre::bail!("Claude Code's config isn't a JSON object");
    };
    let mut changed = false;
    if top.get("hasCompletedOnboarding") != Some(&Value::Bool(true)) {
        top.insert("hasCompletedOnboarding".into(), Value::Bool(true));
        // As its own onboarding leaves it: the theme it would have asked
        // for, unless one is set, and the version it was completed on.
        if !top.contains_key("theme") {
            top.insert("theme".into(), Value::String(CLAUDE_THEME.into()));
        }
        if let (false, Some(v)) = (top.contains_key("lastOnboardingVersion"), version) {
            top.insert("lastOnboardingVersion".into(), Value::String(v.into()));
        }
        changed = true;
    }
    let projects = top
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(projects) = projects else {
        eyre::bail!("Claude Code's config has projects that aren't an object");
    };
    for folder in folders {
        let project = projects
            .entry(folder.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(project) = project else {
            eyre::bail!("Claude Code's config has a project {folder} that isn't an object");
        };
        // A `false` is the CLI's own default, never an answer.
        if project.get("hasTrustDialogAccepted") != Some(&Value::Bool(true)) {
            project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
            changed = true;
        }
    }
    Ok(changed)
}

/// Trusts `folders` in Codex's `doc`, leaving any trust level the person
/// set (an `untrusted` too). Whether it changed anything.
fn settle_codex(doc: &mut toml_edit::DocumentMut, folders: &[String]) -> Result<bool> {
    let root = doc.as_table_mut();
    if !root.contains_key("projects") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        root.insert("projects", toml_edit::Item::Table(t));
    }
    let projects = root["projects"]
        .as_table_like_mut()
        .ok_or_else(|| eyre::eyre!("Codex's config has projects that aren't a table"))?;
    let mut changed = false;
    for folder in folders {
        if projects.get(folder).is_none() {
            projects.insert(folder, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let project = projects
            .get_mut(folder)
            .and_then(|p| p.as_table_like_mut())
            .ok_or_else(|| {
                eyre::eyre!("Codex's config has a project {folder} that isn't a table")
            })?;
        if project.get("trust_level").is_none() {
            project.insert("trust_level", toml_edit::value("trusted"));
            changed = true;
        }
    }
    Ok(changed)
}

/// How long a change waits for the CLI's lock before going on without it
/// (re-reading right before its rename still keeps the CLI's save).
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// A lock untouched for this long is left over: the CLI's lockfile renews
/// its own every 5 s and gives one up after 10 s.
const LOCK_STALE: Duration = Duration::from_secs(20);
/// How many times a change is made again when the file changed under it.
const TRIES: usize = 5;

/// Changes the file at `path` by `change` (its text, none when there is no
/// file, to the new text, none for nothing to change): under its lock, read
/// again right before the new text replaces it, through a temporary file in
/// its folder with its permissions. Whether it changed.
fn update(path: &Path, change: impl Fn(Option<&str>) -> Result<Option<String>>) -> Result<bool> {
    // A link (a dotfiles checkout) is kept: what it points at is changed.
    let target = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).with_context(|| format!("following {}", path.display()))?
        }
        _ => path.to_path_buf(),
    };
    // Nothing missing: no lock, no write. A read caught mid-write (a CLI
    // that doesn't replace it whole) is looked at again under the lock.
    if let Ok(Ok(None)) = read(&target).map(|t| change(t.as_deref())) {
        return Ok(false);
    }
    let dir = target
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no folder", target.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    let _lock = Lock::take(path);
    for _ in 0..TRIES {
        let before = read(&target)?;
        let Some(text) = change(before.as_deref())? else {
            return Ok(false);
        };
        let tmp = write_beside(&target, &text)?;
        if read(&target)? != before {
            let _ = fs::remove_file(&tmp);
            continue;
        }
        fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))?;
        return Ok(true);
    }
    eyre::bail!("{} kept changing while Desk changed it", target.display())
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `text` in a new file beside `target`, with its permissions (`0600` for
/// a new one: it may come to hold a sign-in), flushed to disk.
fn write_beside(target: &Path, text: &str) -> Result<PathBuf> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!(
        ".{name}.desk-{}-{}",
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

/// The CLI's lock on a file: `<file>.lock`, a folder, made with `mkdir`.
struct Lock(Option<PathBuf>);

impl Lock {
    /// Takes the lock, waiting up to [`LOCK_WAIT`] and breaking one left
    /// over; held by someone else all that time, it goes on without.
    fn take(file: &Path) -> Self {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let lock = PathBuf::from(name);
        let until = Instant::now() + LOCK_WAIT;
        loop {
            match fs::create_dir(&lock) {
                Ok(()) => return Self(Some(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|at| SystemTime::now().duration_since(at).ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = fs::remove_dir(&lock);
                        continue;
                    }
                    if Instant::now() >= until {
                        tracing::warn!(lock = %lock.display(), "a CLI's config stayed locked; changing it without the lock");
                        return Self(None);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    tracing::warn!(lock = %lock.display(), error = %e, "couldn't lock a CLI's config");
                    return Self(None);
                }
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(lock) = &self.0 {
            let _ = fs::remove_dir(lock);
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

    #[test]
    fn configs_are_where_the_sessions_environment_says() {
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            claude_config(&env(&[("HOME", "/home/dev")])),
            Some(PathBuf::from("/home/dev/.claude.json"))
        );
        assert_eq!(
            claude_config(&env(&[
                ("HOME", "/home/dev"),
                ("CLAUDE_CONFIG_DIR", "/cfg")
            ])),
            Some(PathBuf::from("/cfg/.claude.json"))
        );
        assert_eq!(claude_config(&env(&[])), None, "never the hub's own home");
        assert_eq!(
            codex_config(&env(&[("HOME", "/home/dev")])),
            Some(PathBuf::from("/home/dev/.codex/config.toml"))
        );
        assert_eq!(
            codex_config(&env(&[("HOME", "/home/dev"), ("CODEX_HOME", "/cx")])),
            Some(PathBuf::from("/cx/config.toml"))
        );
        assert_eq!(codex_config(&env(&[])), None);
    }

    #[test]
    fn a_fresh_home_is_onboarded_and_its_folder_trusted() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-api");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".claude.json");
        assert!(ready_claude(&config, &folder, Some("2.1.318")).unwrap());
        let doc = json_of(&config);
        assert_eq!(doc["hasCompletedOnboarding"], json!(true));
        assert_eq!(doc["theme"], json!("dark"));
        assert_eq!(doc["lastOnboardingVersion"], json!("2.1.318"));
        let real = fs::canonicalize(&folder).unwrap();
        assert_eq!(
            doc["projects"][real.to_str().unwrap()]["hasTrustDialogAccepted"],
            json!(true)
        );
        let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "it may come to hold a sign-in");
        assert!(
            !ready_claude(&config, &folder, Some("2.1.318")).unwrap(),
            "nothing to change the second time"
        );
        assert!(
            !home.path().join(".claude.json.lock").exists(),
            "the lock is let go"
        );
    }

    #[test]
    fn what_the_person_has_is_kept() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-web");
        fs::create_dir(&folder).unwrap();
        let real = fs::canonicalize(&folder).unwrap();
        let real = real.to_str().unwrap();
        let config = home.path().join(".claude.json");
        let before = json!({
            "userID": "u1",
            "numStartups": 7,
            "theme": "light-daltonized",
            "oauthAccount": { "emailAddress": "sam@example.com" },
            "projects": {
                "/elsewhere": { "hasTrustDialogAccepted": false, "allowedTools": ["Bash"] },
                real: { "hasTrustDialogAccepted": false, "lastCost": 1.5 }
            }
        });
        fs::write(&config, serde_json::to_string(&before).unwrap()).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(ready_claude(&config, &folder, None).unwrap());
        let doc = json_of(&config);
        assert_eq!(
            doc["theme"],
            json!("light-daltonized"),
            "the person's theme"
        );
        assert_eq!(doc["hasCompletedOnboarding"], json!(true));
        assert_eq!(doc.get("lastOnboardingVersion"), None, "no version known");
        for k in ["userID", "numStartups", "oauthAccount"] {
            assert_eq!(doc[k], before[k], "{k} is kept");
        }
        assert_eq!(
            doc["projects"]["/elsewhere"],
            before["projects"]["/elsewhere"]
        );
        assert_eq!(doc["projects"][real]["hasTrustDialogAccepted"], json!(true));
        assert_eq!(doc["projects"][real]["lastCost"], json!(1.5));
        let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "its permissions are kept");
    }

    #[test]
    fn onboarding_done_its_theme_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude.json");
        fs::write(&config, r#"{"hasCompletedOnboarding":true}"#).unwrap();
        ready_claude(&config, home.path(), Some("2.1.318")).unwrap();
        let doc = json_of(&config);
        assert_eq!(
            doc.get("theme"),
            None,
            "Claude Code drops its default theme, so adding it would change the file every start"
        );
        assert_eq!(doc.get("lastOnboardingVersion"), None);
    }

    #[test]
    fn a_folder_through_a_link_is_trusted_where_it_resolves() {
        let home = tempfile::tempdir().unwrap();
        let real = home.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config = home.path().join(".claude.json");
        ready_claude(&config, &link, None).unwrap();
        let doc = json_of(&config);
        let resolved = fs::canonicalize(&real).unwrap();
        assert_eq!(
            doc["projects"][resolved.to_str().unwrap()]["hasTrustDialogAccepted"],
            json!(true)
        );
    }

    #[test]
    fn a_linked_config_stays_a_link() {
        let home = tempfile::tempdir().unwrap();
        let kept = home.path().join("dotfiles.json");
        fs::write(&kept, "{}").unwrap();
        let config = home.path().join(".claude.json");
        std::os::unix::fs::symlink(&kept, &config).unwrap();
        ready_claude(&config, home.path(), None).unwrap();
        assert!(fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(json_of(&kept)["hasCompletedOnboarding"], json!(true));
    }

    #[test]
    fn a_config_that_doesnt_parse_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude.json");
        fs::write(&config, r#"{"userID": "u1", "#).unwrap();
        assert!(ready_claude(&config, home.path(), None).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), r#"{"userID": "u1", "#);
        let codex = home.path().join("config.toml");
        fs::write(&codex, "model = [").unwrap();
        assert!(trust_codex(&codex, home.path()).is_err());
        assert_eq!(fs::read_to_string(&codex).unwrap(), "model = [");
    }

    #[test]
    fn a_held_lock_is_waited_for_and_a_left_over_one_broken() {
        let home = tempfile::tempdir().unwrap();
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
        ready_claude(&config, home.path(), None).unwrap();
        saver.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250), "it waited");
        let doc = json_of(&config);
        assert_eq!(doc["numStartups"], json!(3), "the CLI's save is kept");
        assert_eq!(doc["hasCompletedOnboarding"], json!(true));

        // One left over from a CLI that died holding it.
        fs::create_dir(&lock).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        fs::File::open(&lock).unwrap().set_modified(old).unwrap();
        let other = home.path().join("p");
        fs::create_dir(&other).unwrap();
        let started = Instant::now();
        assert!(ready_claude(&config, &other, None).unwrap());
        assert!(
            started.elapsed() < LOCK_WAIT,
            "a left-over lock isn't waited out"
        );
        assert!(!lock.exists());
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".claude.json");
        fs::write(&config, "{}").unwrap();
        // A stand-in CLI saving its counter under its lock, as Claude Code
        // does, while Desk trusts folders.
        let cli = {
            let config = config.clone();
            std::thread::spawn(move || {
                for n in 1..=40 {
                    let _lock = Lock::take(&config);
                    let mut doc = json_of(&config);
                    doc["numStartups"] = json!(n);
                    fs::write(&config, doc.to_string()).unwrap();
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
                        ready_claude(&config, &folder, None).unwrap();
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
                let f = fs::canonicalize(home.path().join(format!("f{i}-{j}"))).unwrap();
                assert_eq!(
                    doc["projects"][f.to_str().unwrap()]["hasTrustDialogAccepted"],
                    json!(true),
                    "{} is trusted",
                    f.display()
                );
            }
        }
    }

    #[test]
    fn codex_trusts_the_folder_and_keeps_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("acme-api");
        fs::create_dir(&folder).unwrap();
        let config = home.path().join(".codex/config.toml");
        assert!(
            trust_codex(&config, &folder).unwrap(),
            "made with its folder"
        );
        let real = fs::canonicalize(&folder).unwrap();
        let doc: toml_edit::DocumentMut = fs::read_to_string(&config).unwrap().parse().unwrap();
        assert_eq!(
            doc["projects"][real.to_str().unwrap()]["trust_level"].as_str(),
            Some("trusted")
        );
        assert!(!trust_codex(&config, &folder).unwrap());

        let kept = "# my settings\nmodel = \"gpt-6\"\n\n[projects.\"/elsewhere\"]\ntrust_level = \"untrusted\"\n";
        fs::write(&config, kept).unwrap();
        trust_codex(&config, &folder).unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(
            text.starts_with(kept),
            "the person's file, comments too, is kept: {text}"
        );
        assert!(text.contains("trust_level = \"trusted\""), "{text}");
        assert!(
            !trust_codex(&config, Path::new("/elsewhere")).unwrap(),
            "a folder the person chose not to trust stays so"
        );
    }
}
