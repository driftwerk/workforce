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

/// The words a setting's name may not have, at any depth, to be written
/// back: whatever runs a command, reaches a server, carries a credential,
/// or widens what Devin may do unasked (its hooks among them: Desk's are in
/// the copy beside the person's). A name's words are its parts split at
/// `_`, `-`, `.`, case (`baseURL`, `URLBase`) and digits (`url2`); a word
/// is one of these, or starts with one of [`NOT_WRITTEN_BACK_ROOTS`], or
/// has one of [`NOT_WRITTEN_BACK_INSIDE`] in it. A session that could write
/// these could make the person's next own Devin run, reach or allow what
/// it chose.
const NOT_WRITTEN_BACK: [&str; 18] = [
    "cmd", "shell", "program", "run", "args", "uri", "base", "server", "servers", "header",
    "headers", "key", "keys", "auth", "mcp", "path", "paths", "env",
];

/// The roots a word of a setting's name may not start with.
const NOT_WRITTEN_BACK_ROOTS: [&str; 24] = [
    "allow",
    "approv",
    "trust",
    "exec",
    "host",
    "endpoint",
    "prox",
    "token",
    "secret",
    "credential",
    "passw",
    "hook",
    "permission",
    "plugin",
    "extension",
    "script",
    "command",
    "sandbox",
    "bypass",
    "cert",
    "tls",
    "otel",
    "environ",
    "auth",
];

/// What a word of a setting's name may not have anywhere in it.
const NOT_WRITTEN_BACK_INSIDE: [&str; 4] = ["url", "uri", "hook", "token"];

/// Whole names never written back, whatever their words.
const NOT_WRITTEN_BACK_NAMED: [&str; 5] = [
    "read_config_from",
    "permission_mode",
    "local_tools_bypass_permissions",
    "editor",
    "statusline",
];

/// Whole names always written back, whatever their words: `keymap` is no
/// key.
const WRITTEN_BACK_NAMED: [&str; 1] = ["keymap"];

/// Permission modes, by any of their names: never a setting's value
/// written back.
const MODES: [&str; 9] = [
    "dangerous",
    "bypass",
    "bypasspermissions",
    "yolo",
    "smart",
    "accept-edits",
    "acceptedits",
    "autonomous",
    "auto",
];

/// A setting's name in words: split at what isn't a letter or digit, from
/// lower to upper case, before the last capital of a run of them followed
/// by a small letter (`URLBase` is `url`, `base`), and between letters and
/// digits.
fn words_of(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            words.push(std::mem::take(&mut word));
            continue;
        }
        if let Some(&prev) = i.checked_sub(1).and_then(|p| chars.get(p)) {
            let next = chars.get(i + 1).copied();
            let split = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && next.is_some_and(char::is_lowercase))
                || (prev.is_alphabetic() && c.is_numeric())
                || (prev.is_numeric() && c.is_alphabetic());
            if split && prev.is_alphanumeric() {
                words.push(std::mem::take(&mut word));
            }
        }
        word.extend(c.to_lowercase());
    }
    words.push(word);
    words.retain(|w| !w.is_empty());
    words
}

/// Whether a setting named `name` is never written back.
fn protected_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if WRITTEN_BACK_NAMED.contains(&lower.as_str()) {
        return false;
    }
    if lower.starts_with("mcp") || NOT_WRITTEN_BACK_NAMED.contains(&lower.as_str()) {
        return true;
    }
    words_of(name).iter().any(|w| {
        NOT_WRITTEN_BACK.contains(&w.as_str())
            || NOT_WRITTEN_BACK_ROOTS.iter().any(|r| w.starts_with(r))
            || NOT_WRITTEN_BACK_INSIDE.iter().any(|r| w.contains(r))
    })
}

/// Whether a setting named `key` set to `v` sets a permission mode: a
/// `…mode…` setting naming one.
fn sets_mode(key: &str, v: &Value) -> bool {
    words_of(key).iter().any(|w| w == "mode")
        && v.as_str()
            .is_some_and(|v| MODES.contains(&v.trim().to_ascii_lowercase().as_str()))
}

/// Whether `v`, a setting's text, reads as somewhere to reach or run: an
/// address, or a path. Under the keymap, a key's slash command is a
/// binding.
fn protected_text(v: &str, in_keymap: bool) -> bool {
    if in_keymap && v.starts_with('/') && !v.starts_with("//") && !v.contains("://") {
        return false;
    }
    v.contains("://") || v.starts_with('/') || v.starts_with('~') || v.starts_with('$')
}

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

/// Reads JSON with comments and trailing commas. Empty text is none.
pub fn parse(text: &str) -> Result<Value> {
    let value: Option<Value> = jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
        .map_err(|e| eyre::eyre!("not JSON: {e}"))?;
    value.ok_or_else(|| eyre::eyre!("empty"))
}

/// A file's text as a config: empty (a file just made) is `{}`.
fn text_or_empty(text: &str) -> &str {
    match text.trim().is_empty() {
        true => "{}\n",
        false => text,
    }
}

/// The person's config as read at a session's start.
#[derive(Debug, Clone, PartialEq)]
pub enum Person {
    /// Their file, read (`{}` when there is none yet, or it is empty), and
    /// where it really is (`real`: none when there is none yet).
    Read {
        config: Value,
        real: Option<PathBuf>,
    },
    /// Their file is there but isn't a config Desk can read: the session
    /// runs without it, and nothing is written back to it.
    Unreadable(String),
}

impl Person {
    pub fn read(path: Option<&Path>) -> Self {
        let empty = || Self::Read {
            config: Value::Object(Map::new()),
            real: None,
        };
        let Some(path) = path else {
            return empty();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => match parse(text_or_empty(&text)) {
                Ok(config @ Value::Object(_)) => match std::fs::canonicalize(path) {
                    Ok(real) => Self::Read {
                        config,
                        real: Some(real),
                    },
                    Err(e) => Self::Unreadable(e.kind().to_string()),
                },
                Ok(_) => Self::Unreadable("it isn't an object".into()),
                Err(e) => Self::Unreadable(e.to_string()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => empty(),
            Err(e) => Self::Unreadable(e.kind().to_string()),
        }
    }

    fn value(&self) -> Value {
        match self {
            Self::Read { config, .. } => config.clone(),
            Self::Unreadable(_) => Value::Object(Map::new()),
        }
    }

    /// What a session's base file keeps of it: the config as copied, and
    /// where it really is. None when nothing is to be written back.
    pub fn base(&self) -> Option<Value> {
        match self {
            Self::Read { config, real } => {
                Some(serde_json::json!({ "config": config, "real": real }))
            }
            Self::Unreadable(_) => None,
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
/// key by key, anything else whole. Settings Desk never writes back are
/// left out, and named in the second list.
pub fn changes(base: &Value, ended: &Value) -> (Vec<Change>, Vec<String>) {
    let mut all = Vec::new();
    diff(&mut Vec::new(), Some(base), Some(ended), &mut all);
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for c in all {
        // A removal is never written back: a copy rewritten without a
        // setting, or cut short, would take the person's with it.
        if c.value.is_none() {
            skipped.push(format!("{} (removed)", c.path.join(".")));
        } else if protected(&c, base) {
            skipped.push(c.path.join("."));
        } else {
            kept.push(c);
        }
    }
    (kept, skipped)
}

/// Whether `change` touches a setting Desk never writes back: by where it
/// is, or by what it puts there or takes away (a whole object with one
/// inside).
fn protected(change: &Change, base: &Value) -> bool {
    fn holds(v: &Value, in_keymap: bool) -> bool {
        match v {
            Value::Object(o) => o
                .iter()
                .any(|(k, v)| protected_name(k) || sets_mode(k, v) || holds(v, in_keymap)),
            Value::Array(a) => a.iter().any(|v| holds(v, in_keymap)),
            Value::String(s) => protected_text(s, in_keymap),
            _ => false,
        }
    }
    let in_keymap = change
        .path
        .first()
        .is_some_and(|k| k.eq_ignore_ascii_case("keymap"));
    let mode = |v: &Value| change.path.last().is_some_and(|k| sets_mode(k, v));
    change.path.iter().any(|k| protected_name(k))
        || change.value.as_ref().is_some_and(mode)
        || at(base, &change.path).is_some_and(mode)
        || change.value.as_ref().is_some_and(|v| holds(v, in_keymap))
        || at(base, &change.path).is_some_and(|v| holds(v, in_keymap))
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
    let (wanted, skipped) = changes(base, ended);
    if !skipped.is_empty() {
        tracing::info!(settings = ?skipped, "Desk doesn't write these back from a Devin session to the person's config");
    }
    if wanted.is_empty() {
        return Ok(None);
    }
    let text = text_or_empty(current.unwrap_or(""));
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
    // What the edit says must be exactly what was meant; if it isn't,
    // nothing is written: never their file rewritten without its comments.
    match parse(&edited) {
        Ok(v) if v == expected => Ok(Some(edited)),
        _ => eyre::bail!("the edit of the person's Devin config didn't read back as meant"),
    }
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
    write_private_with(path, bytes, false)
}

/// [`write_private`]; with `new`, only where nothing is.
fn write_private_with(path: &Path, bytes: &[u8], new: bool) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    match new {
        true => opts.write(true).create_new(true),
        false => opts.write(true).create(true).truncate(true),
    };
    let mut f = opts
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
/// dotfiles), keeping its permissions. `real` is where it really was when
/// the session's copy was made (none: there was none): if it is elsewhere
/// now, or isn't a file of this user's, nothing is written. One made
/// meanwhile is written only when it is a plain file; where there is none,
/// one is made, never over one that appears at the same moment.
///
/// `read` is what it said when the text was made from it (none: there was
/// none): if it says otherwise just before it is replaced (the person
/// saved it meanwhile), nothing is written, so their save stands.
pub fn replace_person_file(
    path: &Path,
    text: &str,
    real: Option<&Path>,
    read: Option<&str>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let now = match std::fs::symlink_metadata(path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Some(now) = now else {
        let dir = path
            .parent()
            .ok_or_else(|| eyre::eyre!("{} has no folder", path.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        let tmp = temp_beside(path)?;
        write_private_with(&tmp, text.as_bytes(), true)?;
        // A link where nothing is: never over a file made meanwhile.
        let made = std::fs::hard_link(&tmp, path);
        let _ = std::fs::remove_file(&tmp);
        return made.with_context(|| format!("making {}", path.display()));
    };
    let target =
        std::fs::canonicalize(path).with_context(|| format!("reading {}", path.display()))?;
    match real {
        Some(was) if was != target => {
            eyre::bail!(
                "the person's Devin config now leads elsewhere than when the session started"
            )
        }
        None if now.file_type().is_symlink() => {
            eyre::bail!("the person's Devin config is a link made while the session ran")
        }
        _ => {}
    }
    let meta = std::fs::metadata(&target)?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != me {
        eyre::bail!("the person's Devin config isn't a file of theirs");
    }
    let mode = meta.permissions().mode() & 0o777;
    let tmp = temp_beside(&target)?;
    write_private_with(&tmp, text.as_bytes(), true)?;
    if std::fs::read_to_string(&target).ok().as_deref() != read {
        let _ = std::fs::remove_file(&tmp);
        eyre::bail!(
            "the person's Devin config changed while it was written back to: theirs stands"
        );
    }
    let done = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .and_then(|()| std::fs::rename(&tmp, &target));
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

/// A name for a file beside `path`, for it to be written whole first.
fn temp_beside(path: &Path) -> Result<PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no folder", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.json".into());
    Ok(dir.join(format!(".{name}.desk-{}", uuid::Uuid::new_v4().simple())))
}

/// Write-backs one after another, so two sessions ending together each
/// read what the other wrote.
static WRITING_BACK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Writes back to the person's file at `person` what Devin saved to a
/// session's copy `ended` (the file's path) since its base (`base`, a
/// file's path: [`Person::base`]). True when it wrote. A session whose base
/// wasn't kept (their config was unreadable) writes nothing; nor does one
/// whose copy is empty, isn't a config, or no longer has Desk's hooks:
/// not what Devin saves, so not to be taken for it.
pub fn write_back(base: &Path, ended: &Path, person: &Path) -> Result<bool> {
    let read = |p: &Path| -> Result<Option<String>> {
        match std::fs::read_to_string(p) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    };
    let (Some(base), Some(ended)) = (read(base)?, read(ended)?) else {
        return Ok(false);
    };
    let base: Value = serde_json::from_str(&base).context("reading a Devin session's base")?;
    let (Some(copied @ Value::Object(_)), real) = (base.get("config"), base.get("real")) else {
        eyre::bail!("a Devin session's base that isn't one");
    };
    let real = real.and_then(Value::as_str).map(PathBuf::from);
    let ended = match parse(&ended) {
        Ok(v @ Value::Object(_)) if has_desk_hooks(&v) => v,
        _ => {
            tracing::warn!("a Devin session's config ended empty, unreadable or without Desk's hooks: nothing of it is written back");
            return Ok(false);
        }
    };
    // A copy with none of the settings the person had but its hooks is
    // one cut short, never what Devin saved. (Any other setting it lost
    // is a removal, never written back: [`changes`].)
    let theirs: Vec<&String> = copied
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .filter(|k| k.as_str() != "hooks")
        .collect();
    if !theirs.is_empty() && theirs.iter().all(|k| ended.get(k.as_str()).is_none()) {
        tracing::warn!("a Devin session's config ended without any of the person's settings: nothing of it is written back");
        return Ok(false);
    }
    let _one = WRITING_BACK.lock();
    let current = match std::fs::read_to_string(person) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", person.display())),
    };
    match written_back(copied, &ended, current.as_deref())? {
        Some(text) => {
            replace_person_file(person, &text, real.as_deref(), current.as_deref())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Whether a config still has Desk's hooks: one whose command posts its
/// event to Desk.
fn has_desk_hooks(config: &Value) -> bool {
    config
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|h| h.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .any(|c| c.contains(crate::devin::EVENT_HEADER))
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
        let desk = format!("curl -H '{}: Stop'", crate::devin::EVENT_HEADER);
        let mut m = Map::new();
        m.insert(
            "Stop".into(),
            json!([{ "hooks": [{ "type": "command", "command": desk }] }]),
        );
        m.insert(
            "PreToolUse".into(),
            json!([{ "hooks": [{ "type": "command", "command": desk }] }]),
        );
        m
    }

    fn read(config: Value) -> Person {
        Person::Read { config, real: None }
    }

    #[test]
    fn the_persons_config_is_copied_whole_and_desks_hooks_follow_theirs() {
        let person = read(parse(THEIRS).unwrap());
        let c = session_config(&person, &desk_hooks());
        assert_eq!(c["otel"]["endpoint"], "http://127.0.0.1:4318");
        assert_eq!(c["agent"]["model"], "swe-1-6-fast");
        let stop = c["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done", "theirs first");
        assert!(has_desk_hooks(&c));
        assert_eq!(c["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        // None of theirs, or one Desk can't read: Desk's hooks alone.
        for p in [
            read(json!({})),
            Person::Unreadable("x".into()),
            read(json!({"hooks": "nonsense"})),
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
        let mut ended = session_config(&read(base.clone()), &desk_hooks());
        ended["agent"]["pinned_models"] = json!(["swe-1-6"]);
        ended["keymap"] = json!({"chat": {"submit": "ctrl+enter"}});
        ended["startup_messages_remaining"] = json!(2);
        ended["otel"]["endpoint"] = json!("http://elsewhere:4318");
        let text = written_back(&base, &ended, Some(THEIRS)).unwrap().unwrap();
        for kept in ["// My Devin.", "// the fast one", "/* keys I rebound */"] {
            assert!(text.contains(kept), "{kept} kept: {text}");
        }
        let v = parse(&text).unwrap();
        assert_eq!(v["agent"]["pinned_models"], json!(["swe-1-6"]));
        assert_eq!(v["agent"]["model"], "swe-1-6-fast");
        assert_eq!(
            v["otel"]["endpoint"], "http://127.0.0.1:4318",
            "never written back"
        );
        assert_eq!(v["keymap"]["chat"]["submit"], "ctrl+enter");
        assert_eq!(v["startup_messages_remaining"], 2);
        // Desk's hooks never reach the person's file.
        assert_eq!(v["hooks"], base["hooks"]);
        // Nothing saved: nothing written.
        assert_eq!(written_back(&base, &base, Some(THEIRS)).unwrap(), None);
        let only_hooks = session_config(&read(base.clone()), &desk_hooks());
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
        let ended = json!({"theme": {"mode": "dark"}});
        assert_eq!(
            written_back(&base, &ended, Some(r#"{"theme": "x"}"#)).unwrap(),
            None
        );
        // Their file gone, or empty: what Devin saved makes it.
        for now in [None, Some(""), Some("\n")] {
            let v = parse(&written_back(&base, &ended, now).unwrap().unwrap()).unwrap();
            assert_eq!(v, json!({"theme": {"mode": "dark"}}), "{now:?}");
        }
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
            let kept = read(base.clone()).base().unwrap();
            write_private(&b, kept.to_string().as_bytes()).unwrap();
            let mut ended = session_config(&read(base.clone()), &desk_hooks());
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
        let was = std::fs::canonicalize(&link).unwrap();
        replace_person_file(&link, "{\"notify\": \"never\"}\n", Some(&was), Some("{}")).unwrap();
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
    fn what_runs_commands_reaches_servers_or_widens_permissions_is_never_written_back() {
        let base = json!({"agent": {"model": "a"}, "mcpServers": {}});
        let ended = json!({
            "agent": {"model": "b", "tools": {"command": "evil"}},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "evil"}]}]},
            "mcpServers": {"x": {"command": "evil"}},
            "mcp_servers": {"y": {"url": "http://evil"}},
            "permission_mode": "dangerous",
            "local_tools_bypass_permissions": true,
            "permissions": {"allow": ["exec(*)"]},
            "env": {"PATH": "/evil"},
            "otel": {"endpoint": "http://evil"},
            "proxy": "http://evil",
            "sandbox": {"enabled": false},
            "read_config_from": {"claude": true},
            "statusline": {"type": "command", "command": "evil"},
            "theme": {"accent": "blue", "headers": {"x": "y"}},
            // Nested, under other names, or as a plain string.
            "agent2": {"permission_mode": "dangerous", "localToolsBypassPermissions": true},
            "tools": {"allowList": ["exec"], "editor": "vim"},
            "api": {"base_url": "x"},
            "editor": "code --wait",
            "plugins": {"x": {"path": "p"}},
            "ui": {"theme": "~/evil.sh", "font": "https://evil/font"},
        });
        let (kept, skipped) = changes(&base, &ended);
        let paths: Vec<String> = kept.iter().map(|c| c.path.join(".")).collect();
        assert_eq!(paths, ["agent.model"], "{skipped:?}");
        // What isn't: a keymap is no key, a theme no host.
        let (kept, _) = changes(
            &json!({}),
            &json!({"keymap": {"chat": {"submit": "ctrl+enter"}}, "theme": "dark",
                "pinned_models": ["swe-1-6"], "notify": "always"}),
        );
        assert_eq!(kept.len(), 4, "{kept:?}");
        // Every way of naming one, and a mode by its value.
        for name in [
            "URLBase",
            "APIKey",
            "OTELEndpoint",
            "url2",
            "server1",
            "hostname",
            "endpoints",
            "urls",
            "tokens",
            "secrets",
            "proxies",
            "allowed",
            "allowlist",
            "allowedTools",
            "trusted",
            "trustedFolders",
            "approval",
            "approvalPolicy",
            "executable",
            "webhook",
            "baseurl",
            "password",
            "passwd",
            "credentials",
            "cert",
            "tls",
            "autoRun",
            "baseURL",
            "accessKey",
        ] {
            assert!(protected_name(name), "{name}");
        }
        for name in [
            "keymap",
            "theme",
            "notify",
            "pinned_models",
            "model",
            "font_size",
        ] {
            assert!(!protected_name(name), "{name}");
        }
        let (kept, _) = changes(
            &json!({"notify": "smart"}),
            &json!({"agent": {"mode": "dangerous"}, "default_mode": "Bypass",
                "notify": "always", "keymap": {"ctrl+k": "/clear"}}),
        );
        let paths: Vec<String> = kept.iter().map(|c| c.path.join(".")).collect();
        assert_eq!(
            paths,
            ["keymap", "notify"],
            "a mode by its value; a binding kept"
        );
        // A removal, at any depth, is never written back.
        let (kept, removed) = changes(
            &json!({"agent": {"model": "a", "pinned_models": ["x"]}, "startup": 1}),
            &json!({"agent": {}, "notify": "always"}),
        );
        let paths: Vec<String> = kept.iter().map(|c| c.path.join(".")).collect();
        assert_eq!(paths, ["notify"]);
        assert_eq!(removed.len(), 3, "{removed:?}");
        for named in ["permission_mode", "statusline", "mcpServers.x", "theme"] {
            assert!(skipped.iter().any(|s| s == named), "{named}: {skipped:?}");
        }
    }

    #[test]
    fn a_session_config_that_isnt_what_devin_saves_writes_nothing_back() {
        let dir = tempfile::tempdir().unwrap();
        let person = dir.path().join("config.json");
        std::fs::write(&person, THEIRS).unwrap();
        let b = dir.path().join("s.base.json");
        let e = dir.path().join("s.devin.json");
        let base = parse(THEIRS).unwrap();
        write_private(
            &b,
            read(base.clone()).base().unwrap().to_string().as_bytes(),
        )
        .unwrap();
        let mut no_hooks = base.clone();
        no_hooks.as_object_mut().unwrap().remove("hooks");
        for ended in [
            String::new(),
            "  \n".into(),
            "{ trunc".into(),
            "[]".into(),
            "null".into(),
            // Every setting gone, Desk's hooks with them: not a save.
            "{}".into(),
            no_hooks.to_string(),
            // Desk's hooks kept, the person's settings gone: not a save.
            serde_json::json!({"hooks": desk_hooks()}).to_string(),
        ] {
            std::fs::write(&e, &ended).unwrap();
            assert!(!write_back(&b, &e, &person).unwrap(), "{ended:?}");
            assert_eq!(std::fs::read_to_string(&person).unwrap(), THEIRS);
        }
    }

    #[test]
    fn the_persons_config_is_written_only_where_it_was_and_only_if_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        std::fs::write(&a, "{}").unwrap();
        std::fs::write(&b, "{}").unwrap();
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let was = std::fs::canonicalize(&link).unwrap();
        // Repointed while the session ran: nothing written, either file.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert!(replace_person_file(&link, "{\"x\": 1}\n", Some(&was), Some("{}")).is_err());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "{}");
        // None at the start, a link made meanwhile: nothing.
        assert!(replace_person_file(&link, "{\"x\": 1}\n", None, Some("{}")).is_err());
        // None at the start, none now: made, the owner's alone.
        let fresh = dir.path().join("devin/config.json");
        replace_person_file(&fresh, "{\"x\": 1}\n", None, None).unwrap();
        // Saved by the person while it was written back to: theirs stands.
        let was = std::fs::canonicalize(&fresh).unwrap();
        assert!(replace_person_file(&fresh, "{\"y\": 2}\n", Some(&was), Some("{}")).is_err());
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "{\"x\": 1}\n");
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A folder where the file was: never replaced.
        let dir_there = dir.path().join("d");
        std::fs::create_dir(&dir_there).unwrap();
        let was = std::fs::canonicalize(&dir_there).unwrap();
        assert!(replace_person_file(&dir_there, "{}", Some(&was), None).is_err());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".desk-"))
            .collect();
        assert!(left.is_empty());
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
