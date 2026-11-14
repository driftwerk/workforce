//! A Devin session's config: the person's own, copied whole, with Desk's
//! hooks appended to theirs.
//!
//! Devin's `--config <path>` replaces `~/.config/devin/config.json`; it
//! doesn't merge (seen on CLI 3000.12.57: a user config's `otel` endpoint
//! was off under `--config` with a file that lacked it). So each session
//! runs on a copy Desk writes. Devin saves to the config it runs on (a
//! `/config` change, a model pinned, a key rebound), so when the session
//! ends what it saved there is written back to the person's own file:
//! only what changed since the copy was made, only where the person
//! hasn't changed the same setting themselves meanwhile, with their
//! comments and layout kept. Nothing else of theirs is ever written.
//!
//! Devin's configs are JSON with comments (docs.devin.ai, config file).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use eyre::{Context, Result};
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::{Map, Value};

/// The settings Desk never writes back: its hooks are in the copy beside
/// the person's, and Devin doesn't save hooks.
const NOT_WRITTEN_BACK: [&str; 1] = ["hooks"];

/// Where the person's own Devin config is: `$XDG_CONFIG_HOME/devin`, else
/// `~/.config/devin` (Devin's docs; on macOS too). None without a home.
pub fn person_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let base = env
        .get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .or_else(|| {
            env.get("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| Path::new(h).join(".config"))
        })?;
    Some(base.join("devin/config.json"))
}

/// Reads JSON with comments and trailing commas.
pub fn parse(text: &str) -> Result<Value> {
    let value: Option<Value> = jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
        .map_err(|e| eyre::eyre!("not JSON: {e}"))?;
    Ok(value.unwrap_or(Value::Object(Map::new())))
}

/// The person's config as read at a session's start.
#[derive(Debug, Clone, PartialEq)]
pub enum Person {
    /// Their file, read (`{}` when there is none yet).
    Read(Value),
    /// Their file is there but isn't a config Desk can read: the session
    /// runs without it, and nothing is written back to it.
    Unreadable(String),
}

impl Person {
    pub fn read(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::Read(Value::Object(Map::new()));
        };
        match std::fs::read_to_string(path) {
            Ok(text) => match parse(&text) {
                Ok(v @ Value::Object(_)) => Self::Read(v),
                Ok(_) => Self::Unreadable("it isn't an object".into()),
                Err(e) => Self::Unreadable(e.to_string()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::Read(Value::Object(Map::new()))
            }
            Err(e) => Self::Unreadable(e.kind().to_string()),
        }
    }

    fn value(&self) -> Value {
        match self {
            Self::Read(v) => v.clone(),
            Self::Unreadable(_) => Value::Object(Map::new()),
        }
    }
}

/// The config a session runs on: the person's whole, with `desk_hooks`
/// (event → matcher groups) appended to theirs per event, so theirs keep
/// running first.
pub fn session_config(person: &Person, desk_hooks: &Map<String, Value>) -> Value {
    let mut config = person.value();
    let root = config.as_object_mut().expect("a config is an object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        // Not hooks Devin could run either; Desk's replace them in its
        // copy only.
        *hooks = Value::Object(Map::new());
    }
    let hooks = hooks.as_object_mut().expect("just made an object");
    for (event, groups) in desk_hooks {
        let theirs = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !theirs.is_array() {
            *theirs = Value::Array(Vec::new());
        }
        let theirs = theirs.as_array_mut().expect("just made an array");
        theirs.extend(groups.as_array().into_iter().flatten().cloned());
    }
    config
}

/// One setting Devin changed in a session's copy: where, and what it is now
/// (`None`: removed).
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: Vec<String>,
    pub value: Option<Value>,
}

/// What differs between `base` (the person's config as copied) and `ended`
/// (the session's copy at its end), setting by setting: objects compared
/// key by key, anything else whole.
pub fn changes(base: &Value, ended: &Value) -> Vec<Change> {
    let mut out = Vec::new();
    diff(&mut Vec::new(), Some(base), Some(ended), &mut out);
    out.retain(|c| {
        !c.path
            .first()
            .is_some_and(|k| NOT_WRITTEN_BACK.contains(&k.as_str()))
    });
    out
}

fn diff(path: &mut Vec<String>, a: Option<&Value>, b: Option<&Value>, out: &mut Vec<Change>) {
    match (a, b) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                path.push(k.clone());
                diff(path, a.get(k), b.get(k), out);
                path.pop();
            }
        }
        (a, b) if a == b => {}
        (_, b) => out.push(Change {
            path: path.clone(),
            value: b.cloned(),
        }),
    }
}

fn at<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(k))
}

/// Whether `change` can go into `current` without overwriting anything the
/// person did: the setting is as it was when the copy was made, and every
/// object on the way to it is an object (or not there yet).
fn applies(change: &Change, base: &Value, current: &Value) -> bool {
    if at(current, &change.path) != at(base, &change.path) {
        return false;
    }
    let mut v = current;
    for k in &change.path[..change.path.len().saturating_sub(1)] {
        match v.get(k) {
            Some(next @ Value::Object(_)) => v = next,
            Some(_) => return false,
            None => return true,
        }
    }
    v.is_object()
}

fn set(v: &mut Value, path: &[String], value: Option<Value>) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut v = v;
    for k in parents {
        let obj = v.as_object_mut().expect("checked by applies");
        v = obj
            .entry(k.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let obj = v.as_object_mut().expect("checked by applies");
    match value {
        Some(value) => {
            obj.insert(last.clone(), value);
        }
        None => {
            obj.remove(last);
        }
    }
}

/// What the person's file should say once what Devin saved in a session is
/// written back: `None` when nothing is to be written. `base` is their
/// config as it was copied, `ended` the session's copy at its end, and
/// `current` their file's text now (`None`: there is none). A setting the
/// person changed themselves since the copy keeps their change.
pub fn written_back(base: &Value, ended: &Value, current: Option<&str>) -> Result<Option<String>> {
    let wanted = changes(base, ended);
    if wanted.is_empty() {
        return Ok(None);
    }
    let text = current.unwrap_or("{}\n");
    let mut expected = parse(text).context("reading the person's Devin config")?;
    if !expected.is_object() {
        eyre::bail!("the person's Devin config isn't an object");
    }
    let apply: Vec<Change> = wanted
        .into_iter()
        .filter(|c| applies(c, base, &expected))
        .collect();
    if apply.is_empty() {
        return Ok(None);
    }
    let root = CstRootNode::parse(text, &ParseOptions::default())
        .map_err(|e| eyre::eyre!("not JSON: {e}"))?;
    let top = root.object_value_or_set();
    for change in &apply {
        edit(&top, &change.path, change.value.as_ref());
        set(&mut expected, &change.path, change.value.clone());
    }
    let edited = root.to_string();
    // What the edit says must be exactly what was meant; if it isn't, the
    // settings are what matter, written plainly.
    let out = match parse(&edited) {
        Ok(v) if v == expected => edited,
        _ => format!("{}\n", serde_json::to_string_pretty(&expected)?),
    };
    Ok(Some(out))
}

fn edit(obj: &CstObject, path: &[String], value: Option<&Value>) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut obj = obj.clone();
    for k in parents {
        match obj.object_value(k) {
            Some(next) => obj = next,
            None if value.is_none() => return,
            None => obj = obj.object_value_or_set(k),
        }
    }
    match (obj.get(last), value) {
        (Some(prop), Some(v)) => prop.set_value(input(v)),
        (None, Some(v)) => {
            obj.append(last, input(v));
        }
        (Some(prop), None) => prop.remove(),
        (None, None) => {}
    }
}

fn input(v: &Value) -> CstInputValue {
    match v {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(a) => CstInputValue::Array(a.iter().map(input).collect()),
        Value::Object(o) => {
            CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), input(v))).collect())
        }
    }
}

/// Writes `bytes` to `path`, readable by this user alone, never through a
/// link planted there.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)?;
    Ok(())
}

/// Replaces the person's file at `path` with `text` at once (a reader
/// never sees half of it), where it really is when it is a link (their
/// dotfiles), keeping its permissions.
pub fn replace_person_file(path: &Path, text: &str) -> Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = real
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no folder", real.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    let mode = std::fs::metadata(&real)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.json".into());
    let tmp = dir.join(format!(".{name}.desk-{}", uuid::Uuid::new_v4().simple()));
    write_private(&tmp, text.as_bytes())?;
    let done = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .and_then(|()| std::fs::rename(&tmp, &real));
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", real.display()));
    }
    Ok(())
}

/// Write-backs one after another, so two sessions ending together each
/// read what the other wrote.
static WRITING_BACK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Writes back to the person's file at `person` what Devin saved to a
/// session's copy `ended` (the file's path) since `base` (the person's
/// config as copied, also a file's path). True when it wrote. A session
/// whose base wasn't kept (their config was unreadable) writes nothing.
pub fn write_back(base: &Path, ended: &Path, person: &Path) -> Result<bool> {
    let read = |p: &Path| -> Result<Option<Value>> {
        match std::fs::read_to_string(p) {
            Ok(t) => parse(&t).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    };
    let (Some(base), Some(ended)) = (read(base)?, read(ended)?) else {
        return Ok(false);
    };
    let _one = WRITING_BACK.lock();
    let current = match std::fs::read_to_string(person) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", person.display())),
    };
    match written_back(&base, &ended, current.as_deref())? {
        Some(text) => {
            replace_person_file(person, &text)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const THEIRS: &str = r#"{
  // My Devin.
  "agent": {
    "model": "swe-1-6-fast", // the fast one
    "pinned_models": [],
  },
  /* keys I rebound */
  "keymap": {},
  "otel": { "endpoint": "http://127.0.0.1:4318" },
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }]
  }
}
"#;

    fn desk_hooks() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "Stop".into(),
            json!([{ "hooks": [{ "type": "command", "command": "desk" }] }]),
        );
        m.insert(
            "PreToolUse".into(),
            json!([{ "hooks": [{ "type": "command", "command": "desk" }] }]),
        );
        m
    }

    #[test]
    fn the_persons_config_is_copied_whole_and_desks_hooks_follow_theirs() {
        let person = Person::Read(parse(THEIRS).unwrap());
        let c = session_config(&person, &desk_hooks());
        assert_eq!(c["otel"]["endpoint"], "http://127.0.0.1:4318");
        assert_eq!(c["agent"]["model"], "swe-1-6-fast");
        let stop = c["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done", "theirs first");
        assert_eq!(stop[1]["hooks"][0]["command"], "desk");
        assert_eq!(c["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        // None of theirs, or one Desk can't read: Desk's hooks alone.
        for p in [
            Person::Read(json!({})),
            Person::Unreadable("x".into()),
            Person::Read(json!({"hooks": "nonsense"})),
        ] {
            let c = session_config(&p, &desk_hooks());
            assert_eq!(c["hooks"]["Stop"].as_array().unwrap().len(), 1, "{p:?}");
        }
    }

    #[test]
    fn the_persons_config_is_found_where_devin_reads_it() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            person_path(&env(&[("HOME", "/Users/sam")])),
            Some(PathBuf::from("/Users/sam/.config/devin/config.json"))
        );
        assert_eq!(
            person_path(&env(&[("HOME", "/Users/sam"), ("XDG_CONFIG_HOME", "/c")])),
            Some(PathBuf::from("/c/devin/config.json"))
        );
        assert_eq!(
            person_path(&env(&[("HOME", "/Users/sam"), ("XDG_CONFIG_HOME", "rel")])),
            Some(PathBuf::from("/Users/sam/.config/devin/config.json"))
        );
        assert_eq!(person_path(&env(&[])), None);
    }

    #[test]
    fn what_devin_saved_is_written_back_with_the_persons_comments_kept() {
        let base = parse(THEIRS).unwrap();
        let mut ended = session_config(&Person::Read(base.clone()), &desk_hooks());
        ended["agent"]["pinned_models"] = json!(["swe-1-6"]);
        ended["keymap"] = json!({"chat": {"submit": "ctrl+enter"}});
        ended["startup_messages_remaining"] = json!(2);
        ended["otel"].as_object_mut().unwrap().remove("endpoint");
        let text = written_back(&base, &ended, Some(THEIRS)).unwrap().unwrap();
        for kept in ["// My Devin.", "// the fast one", "/* keys I rebound */"] {
            assert!(text.contains(kept), "{kept} kept: {text}");
        }
        let v = parse(&text).unwrap();
        assert_eq!(v["agent"]["pinned_models"], json!(["swe-1-6"]));
        assert_eq!(v["agent"]["model"], "swe-1-6-fast");
        assert_eq!(v["keymap"]["chat"]["submit"], "ctrl+enter");
        assert_eq!(v["startup_messages_remaining"], 2);
        assert_eq!(v["otel"], json!({}));
        // Desk's hooks never reach the person's file.
        assert_eq!(v["hooks"], base["hooks"]);
        // Nothing saved: nothing written.
        assert_eq!(written_back(&base, &base, Some(THEIRS)).unwrap(), None);
        let only_hooks = session_config(&Person::Read(base.clone()), &desk_hooks());
        assert_eq!(
            written_back(&base, &only_hooks, Some(THEIRS)).unwrap(),
            None
        );
    }

    #[test]
    fn a_setting_the_person_changed_meanwhile_keeps_their_change() {
        let base = json!({"agent": {"model": "a"}, "notify": "smart"});
        let ended = json!({"agent": {"model": "b"}, "notify": "always"});
        // They chose another model themselves while the session ran.
        let now = r#"{"agent": {"model": "c"}, "notify": "smart"}"#;
        let v = parse(&written_back(&base, &ended, Some(now)).unwrap().unwrap()).unwrap();
        assert_eq!(v, json!({"agent": {"model": "c"}, "notify": "always"}));
        // A setting turned into something else entirely is theirs too.
        let base = json!({});
        let ended = json!({"proxy": {"mode": "off"}});
        assert_eq!(
            written_back(&base, &ended, Some(r#"{"proxy": "x"}"#)).unwrap(),
            None
        );
        // Their file gone: what Devin saved makes a new one.
        let v = parse(&written_back(&base, &ended, None).unwrap().unwrap()).unwrap();
        assert_eq!(v, json!({"proxy": {"mode": "off"}}));
        // A file Desk can't read is never written.
        assert!(written_back(&base, &ended, Some("{ nope")).is_err());
    }

    #[test]
    fn two_sessions_ending_one_after_the_other_both_write_back() {
        let dir = tempfile::tempdir().unwrap();
        let person = dir.path().join("config.json");
        std::fs::write(&person, THEIRS).unwrap();
        std::fs::set_permissions(&person, std::fs::Permissions::from_mode(0o640)).unwrap();
        let base = parse(THEIRS).unwrap();
        let session = |n: &str, change: &dyn Fn(&mut Value)| {
            let b = dir.path().join(format!("{n}.base.json"));
            let e = dir.path().join(format!("{n}.devin.json"));
            write_private(&b, base.to_string().as_bytes()).unwrap();
            let mut ended = session_config(&Person::Read(base.clone()), &desk_hooks());
            change(&mut ended);
            write_private(&e, ended.to_string().as_bytes()).unwrap();
            (b, e)
        };
        let (b1, e1) = session("one", &|v| v["agent"]["model"] = json!("swe-1-6"));
        let (b2, e2) = session("two", &|v| v["notify"] = json!("always"));
        assert!(write_back(&b1, &e1, &person).unwrap());
        assert!(write_back(&b2, &e2, &person).unwrap());
        let text = std::fs::read_to_string(&person).unwrap();
        let v = parse(&text).unwrap();
        assert_eq!(v["agent"]["model"], "swe-1-6", "the first kept");
        assert_eq!(v["notify"], "always");
        assert!(text.contains("// My Devin."));
        let mode = std::fs::metadata(&person).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "its permissions kept");
        // Nothing left beside it.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".desk-"))
            .collect();
        assert!(left.is_empty());
        // No base kept (their config was unreadable at the start): nothing.
        let none = dir.path().join("missing.json");
        assert!(!write_back(&none, &e1, &person).unwrap());
    }

    #[test]
    fn a_linked_config_is_written_where_it_really_is() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-devin.json");
        std::fs::write(&real, "{}").unwrap();
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        replace_person_file(&link, "{\"notify\": \"never\"}\n").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "{\"notify\": \"never\"}\n"
        );
    }

    #[test]
    fn private_files_are_the_owners_alone_and_never_written_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.hook");
        write_private(&f, b"x").unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "theirs").unwrap();
        let planted = dir.path().join("b.hook");
        std::os::unix::fs::symlink(&target, &planted).unwrap();
        assert!(write_private(&planted, b"token").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "theirs");
    }
}
