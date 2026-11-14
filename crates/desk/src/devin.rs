//! Devin: the person's own interactive `devin` TUI, with per-session hooks
//! that report its state to the hub and ask Desk its permission checks,
//! the way Claude Code's do ([`crate::claude`]).
//!
//! Verified on CLI 3000.12.57 and docs.devin.ai (2026-11-14):
//! - its prompt follows `--`;
//! - `--config <path>` replaces the person's config, so Desk runs each
//!   session on a copy with its hooks appended ([`crate::devin_config`]);
//! - its hooks are Claude-format, `command` (and `prompt`) only, with no
//!   `http`; every payload carries `session_id`;
//! - a hook answers with `{"decision":"approve"|"block","reason"}`, and
//!   exit 2 blocks;
//! - matchers are regular expressions on its own tool names (`exec`,
//!   `edit`, `ask_user_question`).
//!
//! Each hook is a `curl` of the session's hook URL. The URL, with its
//! token, is in a curl config file only this user can read, never on a
//! command line `ps` shows. What Devin sends is put in Claude Code's terms
//! ([`normalize`]), so everything after it is the same for both. What
//! Desk answers in Claude Code's terms is put back in Devin's ([`answer`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::Result;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::claude;
use crate::devin_config::{self, Person};
use crate::mode::Mode;
use crate::pty::Spawn;

/// What to run: a new conversation (Devin chooses its id, said at its
/// `SessionStart`), or one to resume.
pub enum Launch<'a> {
    New,
    Resume { session_id: &'a str },
}

pub struct DevinSpawn<'a> {
    pub program: &'a Path,
    pub cwd: &'a Path,
    pub launch: Launch<'a>,
    pub prompt: Option<&'a str>,
    /// The permission mode to start in; none, the person's own.
    pub mode: Option<Mode>,
    pub model: Option<&'a str>,
    /// The session's config ([`SessionFiles::config`]).
    pub config: &'a Path,
    pub env: &'a BTreeMap<String, String>,
}

/// `devin --config <file> [--permission-mode m] [-r id] [--model m] -- <prompt>`.
pub fn spawn(s: &DevinSpawn<'_>) -> Spawn {
    let mut args = vec!["--config".to_string(), s.config.display().to_string()];
    args.extend(s.mode.and_then(Mode::devin_args).unwrap_or_default());
    if let Launch::Resume { session_id } = s.launch {
        args.extend(["-r".to_string(), session_id.to_string()]);
    }
    if let Some(m) = s.model {
        args.extend(["--model".to_string(), m.to_string()]);
    }
    // A bare word is a path for Devin to open: its prompt follows `--`,
    // and nothing in it is read as an option.
    if let Some(p) = s.prompt {
        args.extend(["--".to_string(), p.to_string()]);
    }
    let mut env = s.env.clone();
    scrub_env(&mut env);
    Spawn {
        program: s.program.to_path_buf(),
        args,
        cwd: s.cwd.to_path_buf(),
        env,
    }
}

/// Takes out of a Devin session's environment what would outrank the
/// person's own sign-in (an API key Devin reads from it), and what Desk
/// gives Claude Code only: Devin reads the person's Claude setup, and must
/// not pick up Desk's plugin from it.
pub fn scrub_env(env: &mut BTreeMap<String, String>) {
    env.retain(|k, _| !outranks_sign_in(k));
    for k in OWN_TO_CLAUDE {
        env.remove(k);
    }
}

/// What Desk sets for Claude Code sessions alone.
const OWN_TO_CLAUDE: [&str; 2] = ["CLAUDE_CODE_PLUGIN_DIRS", claude::USAGE_URL];

/// A credential Devin (or Windsurf, whose key it takes) could read from its
/// environment: `WINDSURF_API_KEY`, a `DEVIN_…_TOKEN`.
fn outranks_sign_in(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["DEVIN_", "WINDSURF_", "CODEIUM_"]
        .iter()
        .any(|p| n.starts_with(p))
        && [
            "KEY",
            "TOKEN",
            "SECRET",
            "AUTH",
            "CREDENTIAL",
            "PASSWORD",
            "COOKIE",
        ]
        .iter()
        .any(|w| n.contains(w))
}

/// A session's files in the hub's private run folder: each only this user
/// can read, written when it starts, removed when it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFiles {
    /// The config it runs on: the person's, with Desk's hooks.
    pub config: PathBuf,
    /// The person's config as it was copied, for what Devin saves to be
    /// written back against; none kept when theirs couldn't be read.
    pub base: PathBuf,
    /// The curl config its hooks post with: its hook URL and token.
    pub hook: PathBuf,
}

const CONFIG_SUFFIX: &str = ".devin.json";

impl SessionFiles {
    pub fn of(run_dir: &Path, id: Uuid) -> Self {
        let n = id.simple();
        Self {
            config: run_dir.join(format!("{n}{CONFIG_SUFFIX}")),
            base: run_dir.join(format!("{n}.devin-base.json")),
            hook: run_dir.join(format!("{n}.hook")),
        }
    }

    /// Writes them for a session whose hooks post to `hook_url`, on
    /// `person`'s config. `run_dir` is made the owner's alone first.
    pub fn write(
        &self,
        run_dir: &Path,
        hook_url: &str,
        approval_timeout_secs: u64,
        fail_closed: bool,
        person: &Person,
    ) -> Result<()> {
        crate::token_pipe::private_run_dir(run_dir)?;
        devin_config::write_private(&self.hook, curl_config(hook_url)?.as_bytes())?;
        let hooks = hooks(&self.hook, approval_timeout_secs, fail_closed);
        let config = devin_config::session_config(person, &hooks);
        devin_config::write_private(&self.config, serde_json::to_vec_pretty(&config)?.as_slice())?;
        match person {
            Person::Read(v) => devin_config::write_private(&self.base, v.to_string().as_bytes())?,
            Person::Unreadable(_) => remove(&self.base),
        }
        Ok(())
    }

    /// What Devin saved to the session's config is written back to the
    /// person's file at `person` (none without a home: nothing is), then
    /// the files go. They go even when writing back failed, said in the
    /// result: they hold the session's token.
    pub fn end(&self, person: Option<&Path>) -> Result<bool> {
        let wrote = match person {
            Some(p) => devin_config::write_back(&self.base, &self.config, p),
            None => Ok(false),
        };
        for f in [&self.hook, &self.config, &self.base] {
            remove(f);
        }
        wrote
    }
}

fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "couldn't remove a Devin session's file")
        }
    }
}

/// The sessions whose files are in `run_dir`: at the hub's start, those
/// no longer running are ended ([`SessionFiles::end`]).
pub fn sessions_with_files(run_dir: &Path) -> Vec<Uuid> {
    let Ok(entries) = std::fs::read_dir(run_dir) else {
        return Vec::new();
    };
    let mut ids: Vec<Uuid> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(CONFIG_SUFFIX)
                .or_else(|| name.strip_suffix(".devin-base.json"))
                .or_else(|| name.strip_suffix(".hook"))?;
            Uuid::try_parse(stem).ok()
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// One session `devin list --format json` lists: its id, and when it was
/// made, when it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: String,
    pub created: Option<chrono::DateTime<chrono::Utc>>,
}

/// What `devin list --format json` printed (in a folder: its sessions
/// there). Its entries' shape isn't documented (an empty folder's is
/// `[]`), so an id and a time are looked for under the names they are
/// likely to have. None: not a list Desk can read.
pub fn listed(printed: &str) -> Option<Vec<Listed>> {
    let v: Value = serde_json::from_str(printed.trim()).ok()?;
    let entries = match &v {
        Value::Array(a) => a,
        Value::Object(o) => o.get("sessions").and_then(Value::as_array)?,
        _ => return None,
    };
    Some(
        entries
            .iter()
            .filter_map(|e| {
                let id = text_of(e, &["id", "session_id", "sessionId"])?.to_string();
                let created = [
                    "created_at",
                    "createdAt",
                    "started_at",
                    "startedAt",
                    "created",
                ]
                .iter()
                .find_map(|k| e.get(*k))
                .and_then(time_of);
                Some(Listed { id, created })
            })
            .collect(),
    )
}

fn time_of(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    match v {
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc)),
        // Seconds, or milliseconds, since the epoch.
        Value::Number(n) => {
            let n = n.as_i64()?;
            let secs = if n > 100_000_000_000 { n / 1000 } else { n };
            chrono::DateTime::from_timestamp(secs, 0)
        }
        _ => None,
    }
}

/// The one session in `listed` made after `since` (a Desk session's start
/// that never said its id): none when none or several were, as the
/// person's own `devin` or another Desk session there could otherwise be
/// taken for it.
pub fn made_after(listed: &[Listed], since: chrono::DateTime<chrono::Utc>) -> Option<&str> {
    let mut after = listed
        .iter()
        .filter(|l| l.created.is_some_and(|c| c >= since));
    match (after.next(), after.next()) {
        (Some(one), None) => Some(one.id.as_str()),
        _ => None,
    }
}

/// The curl config a session's hooks post with. curl reads it with `-K`,
/// so the token in its URL is never on a command line.
pub fn curl_config(hook_url: &str) -> Result<String> {
    if hook_url
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        eyre::bail!("a hook URL Desk wouldn't write");
    }
    Ok(format!(
        "url = \"{hook_url}\"\nheader = \"Content-Type: application/json\"\n"
    ))
}

/// The header naming the event a hook posts for: what it is, should a
/// payload not say.
pub const EVENT_HEADER: &str = "X-Desk-Devin-Event";

/// How long its plain hooks may take; Desk answers them at once.
const PLAIN_SECS: u64 = 10;

/// What one hook runs: a `curl` of the session's hook URL (from its
/// `.hook` file) with the payload on stdin, Desk's answer on stdout. With
/// `fail_closed` (a boss on another hub runs it), Desk not answering
/// refuses the call: exit 2, its reason on stderr.
pub fn hook_command(hook_file: &Path, event: &str, timeout: u64, fail_closed: bool) -> String {
    let file = shlex::try_quote(&hook_file.display().to_string())
        .map(|q| q.into_owned())
        .unwrap_or_else(|_| "/dev/null".into());
    let post = format!(
        "curl -sf --max-time {timeout} -K {file} -H '{EVENT_HEADER}: {event}' --data-binary @-"
    );
    match fail_closed {
        true => format!("{post} || {{ echo '{}' >&2; exit 2; }}", claude::UNREACHED),
        false => post,
    }
}

/// The hooks Desk appends to a session's config, event by event.
///
/// One `PreToolUse` entry for every tool, with no matcher (Devin's
/// matchers are regular expressions; an entry for questions beside a
/// catch-all would fire both for a question), waiting as long as a
/// question's start may be held for its boss ([`claude::DIALOG_WAIT_SECS`]):
/// every other call Desk answers at once.
pub fn hooks(
    hook_file: &Path,
    approval_timeout_secs: u64,
    fail_closed: bool,
) -> Map<String, Value> {
    let entry = |event: &str, timeout: u64, fail_closed: bool| {
        let mut hook = json!({
            "type": "command",
            "command": hook_command(hook_file, event, timeout, fail_closed),
            "timeout": timeout + 5,
        });
        if fail_closed {
            hook["fail_closed"] = Value::Bool(true);
        }
        json!([{ "hooks": [hook] }])
    };
    let mut m = Map::new();
    for (event, timeout, closed) in [
        ("SessionStart", PLAIN_SECS, false),
        ("UserPromptSubmit", PLAIN_SECS, false),
        ("PreToolUse", claude::DIALOG_WAIT_SECS, fail_closed),
        ("PermissionRequest", approval_timeout_secs, false),
        ("PostToolUse", PLAIN_SECS, false),
        ("Stop", PLAIN_SECS, false),
        ("SessionEnd", PLAIN_SECS, false),
        // Answered at once, never holding the compaction up.
        ("PreCompact", 5, false),
    ] {
        m.insert(event.into(), entry(event, timeout, closed));
    }
    m
}

/// Reads one of Devin's hooks: its payload put in Claude Code's terms
/// ([`normalize`]), then read as Claude Code's are. `event` is what its
/// [`EVENT_HEADER`] said, for a payload that doesn't.
#[cfg(test)]
pub fn parse_hook(body: &[u8], event: Option<&str>) -> Result<claude::Hook> {
    claude::parse_hook(&normalized(body, event)?)
}

/// A Devin hook's body put in Claude Code's terms ([`normalize`]): what
/// Claude Code's readers ([`claude::parse_hook`], [`crate::mode::of_hook`])
/// read.
pub fn normalized(body: &[u8], event: Option<&str>) -> Result<Vec<u8>> {
    let mut payload: Value = serde_json::from_slice(body)?;
    normalize(&mut payload, event);
    Ok(serde_json::to_vec(&payload)?)
}

/// Where Devin's own name and input for a tool call are kept, beside the
/// input put in Claude Code's terms: what a boss reads as Devin's words.
pub const RAW: &str = "devin";

/// Puts a Devin hook payload in Claude Code's terms: its event named, its
/// tool by Claude Code's name with the input fields Desk reads, a failed
/// tool as `PostToolUseFailure`. Devin's own name and input are kept under
/// [`RAW`] in the input. A tool Desk doesn't know keeps its name.
pub fn normalize(payload: &mut Value, event: Option<&str>) {
    let Some(p) = payload.as_object_mut() else {
        return;
    };
    if !p.get("hook_event_name").is_some_and(Value::is_string) {
        if let Some(e) = event {
            p.insert("hook_event_name".into(), Value::String(e.into()));
        }
    }
    // The mode it says it runs in, by Claude Code's name.
    if let Some(mode) = p.get("permission_mode").and_then(Value::as_str) {
        let named = match mode {
            "normal" | "auto" => Some(Mode::Default),
            "accept-edits" => Some(Mode::AcceptEdits),
            "smart" => Some(Mode::Auto),
            "dangerous" | "yolo" | "bypass" => Some(Mode::BypassPermissions),
            _ => None,
        };
        match named {
            Some(m) => p.insert("permission_mode".into(), m.as_str().into()),
            None => p.remove("permission_mode"),
        };
    }
    // Devin's call id, should it send one under another name.
    if !p.contains_key("tool_use_id") {
        if let Some(id) = ["tool_call_id", "call_id"]
            .iter()
            .find_map(|k| p.get(*k).filter(|v| v.is_string()).cloned())
        {
            p.insert("tool_use_id".into(), id);
        }
    }
    if p.get("hook_event_name").and_then(Value::as_str) == Some("PostToolUse") {
        let response = p.get("tool_response");
        if response
            .and_then(|r| r.get("success"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            let error = response
                .and_then(|r| r.get("error"))
                .and_then(Value::as_str)
                .filter(|e| !e.trim().is_empty())
                .unwrap_or("it failed")
                .to_string();
            p.insert("hook_event_name".into(), "PostToolUseFailure".into());
            p.insert("error".into(), Value::String(error));
        }
    }
    let Some(name) = p
        .get("tool_name")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let raw = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some((tool, mut input)) = tool_of(&name, &raw) else {
        return;
    };
    input.insert(RAW.into(), json!({ "tool": name, "input": raw }));
    p.insert("tool_name".into(), Value::String(tool.into()));
    p.insert("tool_input".into(), Value::Object(input));
}

/// The first of `keys` `input` has as text.
fn text_of<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
}

/// What Claude Code calls Devin's tool `name`, and the input fields Desk
/// reads, from Devin's `input`. Its documented names (docs.devin.ai,
/// lifecycle hooks); its input fields only `exec`'s (`command`,
/// `shell_id`) are documented, so each field is looked for under the
/// names it is likely to have. None: a tool Desk doesn't know, which keeps
/// its own name (an MCP server's `mcp__s__t` among them).
fn tool_of(name: &str, input: &Value) -> Option<(&'static str, Map<String, Value>)> {
    let mut out = Map::new();
    let mut put = |k: &str, v: Option<&str>| {
        if let Some(v) = v {
            out.insert(k.into(), Value::String(v.into()));
        }
    };
    const PATH: [&str; 4] = ["file_path", "path", "file", "filename"];
    let tool = match name {
        "exec" => {
            put("command", text_of(input, &["command", "cmd"]));
            "Bash"
        }
        // What it types into a running command runs there too.
        "write_to_process" => {
            put(
                "command",
                text_of(input, &["input", "text", "data", "command"]),
            );
            "Bash"
        }
        "edit" => {
            put("file_path", text_of(input, &PATH));
            put("old_string", text_of(input, &["old_string", "old_str"]));
            put("new_string", text_of(input, &["new_string", "new_str"]));
            "Edit"
        }
        "write" => {
            put("file_path", text_of(input, &PATH));
            "Write"
        }
        // Every file the patch touches, as Codex's changes name them.
        "apply_patch" => {
            let patch = text_of(input, &["patch", "input", "diff", "content"]);
            put("patch", patch);
            let paths = patch.map(patch_paths).unwrap_or_default();
            out.insert("paths".into(), json!(paths));
            "Edit"
        }
        "notebook_edit" => {
            put(
                "notebook_path",
                text_of(input, &["notebook_path", "path", "file_path"]),
            );
            "NotebookEdit"
        }
        "read" | "notebook_read" => {
            put("file_path", text_of(input, &PATH));
            if name == "notebook_read" {
                "NotebookRead"
            } else {
                "Read"
            }
        }
        "grep" | "glob" => {
            put("pattern", text_of(input, &["pattern", "query", "regex"]));
            put("path", text_of(input, &["path", "dir", "directory"]));
            put("glob", text_of(input, &["glob", "include"]));
            if name == "grep" {
                "Grep"
            } else {
                "Glob"
            }
        }
        "webfetch" => {
            put("url", text_of(input, &["url"]));
            "WebFetch"
        }
        "todo_write" => "TodoWrite",
        "run_subagent" | "read_subagent" => "Task",
        "skill" => "Skill",
        "ask_user_question" => {
            if let Some(q) = questions_of(input) {
                out.insert("questions".into(), q);
            }
            claude::DIALOG_TOOL
        }
        "exit_plan_mode" => {
            put("plan", text_of(input, &["plan", "content", "text"]));
            claude::PLAN_TOOL
        }
        _ => return None,
    };
    Some((tool, out))
}

/// The files a patch in `*** Begin Patch` form names: each it adds,
/// updates, deletes or moves to.
pub fn patch_paths(patch: &str) -> Vec<String> {
    const MARKS: [&str; 4] = [
        "*** Add File:",
        "*** Update File:",
        "*** Delete File:",
        "*** Move to:",
    ];
    let mut paths: Vec<String> = Vec::new();
    for line in patch.lines() {
        let line = line.trim_end();
        if let Some(p) = MARKS.iter().find_map(|m| line.strip_prefix(m)) {
            let p = p.trim().to_string();
            if !p.is_empty() && !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    paths
}

/// `ask_user_question`'s questions as Claude Code's `AskUserQuestion` has
/// them (`question`, `header`, `options[].label`, `multiSelect`): its own
/// shape isn't documented, so a lone question, options as plain words and
/// the like are read too.
fn questions_of(input: &Value) -> Option<Value> {
    let one = |q: &Value| -> Option<Value> {
        let text = match q {
            Value::String(s) => Some(s.as_str()),
            q => text_of(q, &["question", "text", "prompt", "title"]),
        }?;
        let options: Vec<Value> = q
            .get("options")
            .or_else(|| q.get("choices"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| match o {
                Value::String(s) => Some(json!({ "label": s })),
                o => text_of(o, &["label", "text", "value", "title"]).map(|l| {
                    let mut opt = json!({ "label": l });
                    if let Some(d) = text_of(o, &["description"]) {
                        opt["description"] = json!(d);
                    }
                    opt
                }),
            })
            .collect();
        let mut out = json!({ "question": text, "options": options });
        if let Some(h) = text_of(q, &["header"]) {
            out["header"] = json!(h);
        }
        let multi = ["multiSelect", "multi_select", "multiple", "allow_multiple"]
            .iter()
            .find_map(|k| q.get(*k).and_then(Value::as_bool));
        out["multiSelect"] = json!(multi.unwrap_or(false));
        Some(out)
    };
    let questions: Vec<Value> = match input.get("questions").and_then(Value::as_array) {
        Some(qs) => qs.iter().filter_map(one).collect(),
        None => one(input).into_iter().collect(),
    };
    (!questions.is_empty()).then_some(Value::Array(questions))
}

/// Desk's answer to a hook, made in Claude Code's terms, in Devin's: an
/// allow approves, a deny blocks with its reason, an ask or no decision is
/// none (its terminal asks), and words for the model beside a result stay
/// as they are (Devin reads `additionalContext` there).
pub fn answer(claude_said: Value) -> Value {
    let none = || json!({});
    let Some(out) = claude_said.get("hookSpecificOutput") else {
        return none();
    };
    let text = |k: &str| out.get(k).and_then(Value::as_str);
    let block = |reason: Option<&str>| json!({ "decision": "block", "reason": reason.filter(|r| !r.is_empty()).unwrap_or("Denied from Desk.") });
    // A permission check's (`PermissionRequest`).
    if let Some(d) = out.get("decision") {
        return match d.get("behavior").and_then(Value::as_str) {
            Some("allow") => json!({ "decision": "approve" }),
            Some("deny") => block(d.get("message").and_then(Value::as_str)),
            _ => none(),
        };
    }
    // A call's start's (`PreToolUse`).
    if let Some(d) = text("permissionDecision") {
        return match d {
            "allow" => json!({ "decision": "approve" }),
            "deny" => block(text("permissionDecisionReason")),
            _ => none(),
        };
    }
    if let Some(words) = text("additionalContext") {
        let event = match text("hookEventName") {
            // Devin's failed call is its `PostToolUse` ([`normalize`]).
            Some("PostToolUseFailure") | None => "PostToolUse",
            Some(e) => e,
        };
        return json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": words } });
    }
    none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::{Hook, Question};

    fn files() -> (tempfile::TempDir, SessionFiles) {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let f = SessionFiles::of(&run, Uuid::nil());
        (dir, f)
    }

    const URL: &str = "http://127.0.0.1:50999/hook/0000/5ecret0token";

    fn spawned(launch: Launch<'_>, prompt: Option<&str>, mode: Option<Mode>) -> Spawn {
        let env = BTreeMap::from([
            ("HOME".to_string(), "/Users/sam".to_string()),
            ("WINDSURF_API_KEY".to_string(), "k".to_string()),
            ("DEVIN_API_TOKEN".to_string(), "t".to_string()),
            ("DEVIN_MODEL".to_string(), "swe-1-6".to_string()),
            ("CLAUDE_CODE_PLUGIN_DIRS".to_string(), "/p".to_string()),
            ("COLORFGBG".to_string(), "0;15".to_string()),
        ]);
        spawn(&DevinSpawn {
            program: Path::new("/Users/sam/.local/bin/devin"),
            cwd: Path::new("/repo"),
            launch,
            prompt,
            mode,
            model: None,
            config: Path::new("/run/0.devin.json"),
            env: &env,
        })
    }

    #[test]
    fn its_prompt_follows_two_dashes_and_a_resume_names_the_session() {
        let s = spawned(Launch::New, Some("--config /tmp/evil.json"), None);
        assert_eq!(
            s.args,
            [
                "--config",
                "/run/0.devin.json",
                "--",
                "--config /tmp/evil.json"
            ]
        );
        let s = spawned(
            Launch::Resume {
                session_id: "ses_1",
            },
            Some("go on"),
            Some(Mode::AcceptEdits),
        );
        assert_eq!(
            s.args,
            [
                "--config",
                "/run/0.devin.json",
                "--permission-mode",
                "accept-edits",
                "-r",
                "ses_1",
                "--",
                "go on"
            ]
        );
        let s = spawned(Launch::New, None, Some(Mode::Plan));
        assert_eq!(
            s.args,
            ["--config", "/run/0.devin.json"],
            "plan has no flag"
        );
        assert!(!s.args.iter().any(|a| a == "--"));
    }

    #[test]
    fn its_environment_holds_no_key_that_outranks_the_persons_sign_in() {
        let s = spawned(Launch::New, None, None);
        for gone in [
            "WINDSURF_API_KEY",
            "DEVIN_API_TOKEN",
            "CLAUDE_CODE_PLUGIN_DIRS",
        ] {
            assert!(!s.env.contains_key(gone), "{gone}");
        }
        for kept in ["HOME", "DEVIN_MODEL", "COLORFGBG"] {
            assert!(s.env.contains_key(kept), "{kept}");
        }
    }

    #[test]
    fn modes_start_as_devin_names_them_and_plan_cant() {
        let arg = |m: Mode| m.devin_args().map(|a| a[1].clone());
        assert_eq!(arg(Mode::Default).as_deref(), Some("normal"));
        assert_eq!(arg(Mode::AcceptEdits).as_deref(), Some("accept-edits"));
        assert_eq!(arg(Mode::Auto).as_deref(), Some("smart"));
        assert_eq!(arg(Mode::BypassPermissions).as_deref(), Some("dangerous"));
        assert_eq!(Mode::Plan.devin_args(), None);
    }

    #[test]
    fn a_sessions_files_are_its_own_and_its_token_is_on_no_command_line() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, f) = files();
        let run = f.config.parent().unwrap().to_path_buf();
        let person = Person::Read(json!({"agent": {"model": "swe-1-6-fast"},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}));
        f.write(&run, URL, 3600, false, &person).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&run), 0o700);
        for p in [&f.config, &f.base, &f.hook] {
            assert_eq!(mode(p), 0o600, "{}", p.display());
        }
        let config: Value = serde_json::from_slice(&std::fs::read(&f.config).unwrap()).unwrap();
        assert_eq!(config["agent"]["model"], "swe-1-6-fast");
        assert_eq!(
            config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "say done"
        );
        let token = "5ecret0token";
        let hook_text = std::fs::read_to_string(&f.hook).unwrap();
        assert!(hook_text.contains(token));
        // The token is in the curl file alone: not in any hook's command,
        // nor in the arguments.
        let config_text = std::fs::read_to_string(&f.config).unwrap();
        assert!(!config_text.contains(token), "{config_text}");
        for (event, groups) in config["hooks"].as_object().unwrap() {
            for g in groups.as_array().unwrap() {
                for h in g["hooks"].as_array().unwrap() {
                    let c = h["command"].as_str().unwrap();
                    assert!(!c.contains(token), "{event}: {c}");
                }
            }
        }
        let s = spawn(&DevinSpawn {
            program: Path::new("/bin/devin"),
            cwd: Path::new("/repo"),
            launch: Launch::New,
            prompt: Some("hi"),
            mode: None,
            model: None,
            config: &f.config,
            env: &BTreeMap::new(),
        });
        assert!(!s.args.iter().any(|a| a.contains(token)));
        assert_eq!(sessions_with_files(&run), vec![Uuid::nil()]);
        // Ended: written back (nothing saved here), every file gone.
        let person_file = run.join("person.json");
        assert!(!f.end(Some(&person_file)).unwrap());
        for p in [&f.config, &f.base, &f.hook] {
            assert!(!p.exists(), "{}", p.display());
        }
        assert!(sessions_with_files(&run).is_empty());
        // An unreadable config keeps no base: nothing is written back.
        f.write(&run, URL, 3600, false, &Person::Unreadable("x".into()))
            .unwrap();
        assert!(!f.base.exists());
        assert!(curl_config("http://h/\"\nurl = \"http://evil").is_err());
    }

    #[test]
    fn every_hook_posts_through_the_curl_file_and_waits_as_long_as_its_event_may() {
        let hooks = hooks(Path::new("/run dir/0.hook"), 3600, false);
        let events: Vec<&str> = hooks.keys().map(String::as_str).collect();
        for e in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
            "SessionEnd",
            "PreCompact",
        ] {
            assert!(events.contains(&e), "{e}");
            let groups = hooks[e].as_array().unwrap();
            assert_eq!(groups.len(), 1, "one entry: a question never fires two");
            assert!(groups[0].get("matcher").is_none(), "every tool");
            let h = &groups[0]["hooks"][0];
            assert_eq!(h["type"], "command");
            let c = h["command"].as_str().unwrap();
            assert!(c.starts_with("curl -sf --max-time "), "{c}");
            assert!(c.contains("-K '/run dir/0.hook'"), "{c}");
            assert!(c.contains(&format!("{EVENT_HEADER}: {e}")), "{c}");
            assert!(h.get("fail_closed").is_none());
        }
        let timeout = |e: &str| hooks[e][0]["hooks"][0]["timeout"].as_u64().unwrap();
        assert_eq!(timeout("PermissionRequest"), 3605);
        assert_eq!(timeout("PreToolUse"), claude::DIALOG_WAIT_SECS + 5);
        assert_eq!(timeout("Stop"), 15);
        let closed = super::hooks(Path::new("/r/0.hook"), 60, true);
        let pre = &closed["PreToolUse"][0]["hooks"][0];
        assert_eq!(pre["fail_closed"], true);
        assert!(pre["command"].as_str().unwrap().ends_with("exit 2; }"));
        assert!(closed["Stop"][0]["hooks"][0].get("fail_closed").is_none());
    }

    /// The pre-tool hook of a session a served boss would run: Desk not
    /// answering refuses the call (exit 2, why on stderr); Desk answering
    /// is the hook's answer. Its token reaches the hub from the file.
    #[test]
    fn a_fail_closed_hook_refuses_a_call_desk_doesnt_answer() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let hook_file = dir.path().join("0.hook");
        let run = |url: &str| {
            std::fs::write(&hook_file, curl_config(url).unwrap()).unwrap();
            let c = hook_command(&hook_file, "PreToolUse", 3, true);
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(&c)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(br#"{"tool_name":"exec","tool_input":{"command":"ls"}}"#)
                .unwrap();
            child.wait_with_output().unwrap()
        };
        let out = run("http://127.0.0.1:9/hook/a/b");
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains(claude::UNREACHED));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = c.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let said = r#"{"decision":"block","reason":"not judged safe"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{said}",
                said.len()
            );
            c.write_all(reply.as_bytes()).unwrap();
            request
        });
        let out = run(&format!("http://127.0.0.1:{port}/hook/a/5ecret"));
        let request = served.join().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert!(request.starts_with("POST /hook/a/5ecret "), "{request}");
        assert!(request.contains("Content-Type: application/json"));
        assert!(request.contains(&format!("{EVENT_HEADER}: PreToolUse")));
        let said: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(said["decision"], "block");
    }

    fn parsed(v: Value) -> Hook {
        parse_hook(v.to_string().as_bytes(), None).unwrap()
    }

    #[test]
    fn each_of_devins_tools_reads_as_claude_codes() {
        let start = |tool: &str, input: Value| -> (String, Value) {
            let mut p = json!({"hook_event_name": "PreToolUse", "session_id": "s",
                "prompt_id": "p", "tool_name": tool, "tool_input": input});
            normalize(&mut p, None);
            (
                p["tool_name"].as_str().unwrap().to_string(),
                p["tool_input"].clone(),
            )
        };
        for (devin, input, claude, field, value) in [
            (
                "exec",
                json!({"command": "cargo test", "shell_id": "1"}),
                "Bash",
                "command",
                "cargo test",
            ),
            (
                "write_to_process",
                json!({"shell_id": "1", "input": "y\n"}),
                "Bash",
                "command",
                "y\n",
            ),
            (
                "edit",
                json!({"file_path": "src/a.rs", "old_str": "a", "new_str": "b"}),
                "Edit",
                "file_path",
                "src/a.rs",
            ),
            (
                "edit",
                json!({"path": "src/b.rs"}),
                "Edit",
                "file_path",
                "src/b.rs",
            ),
            (
                "write",
                json!({"file_path": "n.md", "content": "x"}),
                "Write",
                "file_path",
                "n.md",
            ),
            (
                "read",
                json!({"file_path": "/etc/hosts"}),
                "Read",
                "file_path",
                "/etc/hosts",
            ),
            (
                "grep",
                json!({"pattern": "fn main", "path": "src"}),
                "Grep",
                "pattern",
                "fn main",
            ),
            (
                "glob",
                json!({"pattern": "**/*.rs"}),
                "Glob",
                "pattern",
                "**/*.rs",
            ),
            (
                "webfetch",
                json!({"url": "https://docs.devin.ai"}),
                "WebFetch",
                "url",
                "https://docs.devin.ai",
            ),
            (
                "notebook_edit",
                json!({"notebook_path": "a.ipynb"}),
                "NotebookEdit",
                "notebook_path",
                "a.ipynb",
            ),
            (
                "exit_plan_mode",
                json!({"plan": "# Do it"}),
                "ExitPlanMode",
                "plan",
                "# Do it",
            ),
        ] {
            let (tool, normalized) = start(devin, input.clone());
            assert_eq!(tool, claude, "{devin}");
            assert_eq!(normalized[field], value, "{devin}");
            assert_eq!(
                normalized[RAW],
                json!({"tool": devin, "input": input}),
                "{devin} kept"
            );
        }
        for (devin, claude) in [
            ("todo_write", "TodoWrite"),
            ("run_subagent", "Task"),
            ("read_subagent", "Task"),
            ("skill", "Skill"),
        ] {
            assert_eq!(start(devin, json!({})).0, claude);
        }
        // Unknown to Desk: its own name, its input untouched.
        for devin in ["request_scope", "mcp__github__create_issue", "kill_shell"] {
            let (tool, input) = start(devin, json!({"a": 1}));
            assert_eq!((tool.as_str(), input), (devin, json!({"a": 1})));
        }
        // And it reads on as Claude Code's do.
        assert_eq!(
            parsed(json!({"hook_event_name": "PreToolUse", "tool_name": "exec",
                "tool_input": {"command": "ls src", "shell_id": "1"}})),
            Hook::ToolStart {
                tool: "Bash(ls src)".into(),
                call: crate::asking::Call {
                    tool: "Bash".into(),
                    input: Some("ls src".into())
                },
                id: None,
                input: json!({"command": "ls src",
                    "devin": {"tool": "exec", "input": {"command": "ls src", "shell_id": "1"}}}),
            }
        );
    }

    #[test]
    fn its_sessions_are_read_off_its_list() {
        assert_eq!(listed("[]"), Some(vec![]));
        assert_eq!(listed("not json"), None);
        let printed = r#"[{"id":"ses_a","created_at":"2026-11-14T09:00:00Z","title":"x"},
            {"session_id":"ses_b","createdAt":1794648600000},{"title":"no id"}]"#;
        let l = listed(printed).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].id, "ses_a");
        assert_eq!(l[1].created.unwrap().timestamp(), 1_794_648_600);
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        // Exactly one made since: that one; none or two: neither.
        assert_eq!(made_after(&l, at("2026-11-14T08:59:00Z")), None, "two");
        let one = vec![
            Listed {
                id: "ses_a".into(),
                created: Some(at("2026-11-14T09:00:00Z")),
            },
            Listed {
                id: "old".into(),
                created: Some(at("2026-11-01T09:00:00Z")),
            },
            Listed {
                id: "untimed".into(),
                created: None,
            },
        ];
        assert_eq!(made_after(&one, at("2026-11-14T08:59:00Z")), Some("ses_a"));
        assert_eq!(made_after(&one, at("2026-11-14T10:00:00Z")), None);
        assert_eq!(listed(r#"{"sessions":[{"id":"x"}]}"#).unwrap()[0].id, "x");
    }

    #[test]
    fn the_mode_it_says_reads_by_claude_codes_names() {
        let mode = |m: &str| {
            let n = normalized(
                json!({"hook_event_name": "Stop", "permission_mode": m})
                    .to_string()
                    .as_bytes(),
                None,
            )
            .unwrap();
            crate::mode::of_hook(&n)
        };
        assert_eq!(mode("normal"), Some(Mode::Default));
        assert_eq!(mode("accept-edits"), Some(Mode::AcceptEdits));
        assert_eq!(mode("smart"), Some(Mode::Auto));
        assert_eq!(mode("dangerous"), Some(Mode::BypassPermissions));
        assert_eq!(mode("autonomous"), None);
    }

    #[test]
    fn a_patch_names_every_file_it_touches() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** Add File: src/new.rs\n+x\n\
                     *** Delete File: old.rs\n*** Update File: src/c.rs\n*** Move to: src/d.rs\n*** Update File: src/a.rs\n*** End Patch\n";
        assert_eq!(
            patch_paths(patch),
            ["src/a.rs", "src/new.rs", "old.rs", "src/c.rs", "src/d.rs"]
        );
        let mut p = json!({"hook_event_name": "PermissionRequest", "tool_name": "apply_patch",
            "tool_input": {"patch": patch}});
        normalize(&mut p, None);
        assert_eq!(p["tool_name"], "Edit");
        assert_eq!(p["tool_input"]["paths"].as_array().unwrap().len(), 5);
        // None named: none, which a boss can't judge.
        let mut p = json!({"tool_name": "apply_patch", "tool_input": {}});
        normalize(&mut p, None);
        assert_eq!(p["tool_input"]["paths"], json!([]));
    }

    #[test]
    fn its_events_read_as_claude_codes() {
        assert_eq!(
            parsed(
                json!({"hook_event_name": "SessionStart", "session_id": "ses_9", "source": "resume"})
            ),
            Hook::SessionStart {
                session_id: Some("ses_9".into()),
                transcript: None,
                resumed: true
            }
        );
        // Its event from the header, when a payload doesn't say.
        assert!(matches!(
            parse_hook(
                br#"{"session_id":"ses_9","source":"startup"}"#,
                Some("SessionStart")
            )
            .unwrap(),
            Hook::SessionStart { resumed: false, .. }
        ));
        assert_eq!(
            parsed(
                json!({"hook_event_name": "Stop", "session_id": "s", "stop_hook_active": false,
                "last_assistant_message": "Done: the tests pass."})
            ),
            Hook::Stop {
                transcript: None,
                reply: Some("Done: the tests pass.".into()),
                background: None
            }
        );
        assert_eq!(
            parsed(json!({"hook_event_name": "UserPromptSubmit", "prompt": "fix it"})),
            Hook::Prompt("fix it".into())
        );
        assert!(matches!(
            parsed(json!({"hook_event_name": "PermissionRequest", "tool_name": "exec",
                "tool_input": {"command": "rm -rf build"}})),
            Hook::Permission { tool, request, .. } if tool == "Bash(rm -rf build)" && request.tool == "Bash"
        ));
        // A failed tool is Claude Code's failure; one that worked, its end.
        assert_eq!(
            parsed(
                json!({"hook_event_name": "PostToolUse", "tool_name": "exec",
                "tool_input": {"command": "cargo test"},
                "tool_response": {"success": false, "output": "", "error": "exit 101"}})
            ),
            Hook::ToolEnd {
                id: None,
                interrupt: false,
                failed: Some("Bash(cargo test): exit 101".into())
            }
        );
        assert_eq!(
            parsed(
                json!({"hook_event_name": "PostToolUse", "tool_name": "read", "tool_call_id": "c1",
                "tool_input": {"file_path": "a"}, "tool_response": {"success": true, "output": "x", "error": null}})
            ),
            Hook::ToolEnd {
                id: Some("c1".into()),
                interrupt: false,
                failed: None
            }
        );
        assert_eq!(
            parsed(json!({"hook_event_name": "SessionEnd", "reason": "exit"})),
            Hook::SessionEnd
        );
    }

    #[test]
    fn its_question_reads_as_claude_codes_whatever_its_shape() {
        let asked = |input: Value| match parsed(json!({"hook_event_name": "PreToolUse",
            "tool_name": "ask_user_question", "tool_input": input}))
        {
            Hook::Question { question, asks, .. } => (question, asks.is_some()),
            other => panic!("not a question: {other:?}"),
        };
        let want = Question {
            tool: "Question".into(),
            detail: "Which colour?".into(),
            why: None,
            options: Some(vec!["Red".into(), "Green".into()]),
        };
        // As Claude Code's.
        assert_eq!(
            asked(
                json!({"questions": [{"question": "Which colour?", "header": "Colour",
                "options": [{"label": "Red"}, {"label": "Green"}], "multiSelect": false}]})
            ),
            (want.clone(), true)
        );
        // A lone question with plain options.
        assert_eq!(
            asked(json!({"question": "Which colour?", "options": ["Red", "Green"]})),
            (want, true)
        );
    }

    #[test]
    fn every_answer_desk_makes_reads_as_devin_reads_one() {
        let approve = json!({"decision": "approve"});
        let none = json!({});
        for (made, devin) in [
            (claude::allow(), approve.clone()),
            (claude::plan_approved(), approve.clone()),
            (claude::allow_call(), approve.clone()),
            (
                claude::deny(),
                json!({"decision": "block", "reason": "Denied from Desk."}),
            ),
            (
                claude::deny_saying("Use the test database."),
                json!({"decision": "block", "reason": "Use the test database."}),
            ),
            (
                claude::deny_call("Held by Desk's breaker."),
                json!({"decision": "block", "reason": "Held by Desk's breaker."}),
            ),
            (
                claude::answered_for("The person answered: Green"),
                json!({"decision": "block", "reason": "The person answered: Green"}),
            ),
            (claude::ask_call("asked"), none.clone()),
            (claude::ask_in_terminal(), none.clone()),
            (
                claude::context("PostToolUseFailure", "Try the other port."),
                json!({"hookSpecificOutput": {"hookEventName": "PostToolUse",
                    "additionalContext": "Try the other port."}}),
            ),
            (
                claude::context("PostToolUse", "ok"),
                json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "ok"}}),
            ),
            (json!({}), none.clone()),
            (json!({"anything": 1}), none.clone()),
        ] {
            assert_eq!(answer(made.clone()), devin, "{made}");
        }
    }
}
