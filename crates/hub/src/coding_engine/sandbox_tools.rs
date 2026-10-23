//! Claude Code's six base tools, implemented over a [`CodingSandbox`] so the
//! hub loop can code wherever a round runs, a local workdir or a scope on the
//! worker, through one set of paths and rules. Paths are relative to the
//! workspace or absolute inside it; the sandbox holds the boundary, so these
//! tools never touch the hub's own filesystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::coding_sandbox::{CodingSandbox, ExecOutcome, SandboxCommand};

/// The most of a file `Read` and `Edit` take in.
const FILE_MAX_BYTES: usize = 2 * 1024 * 1024;
const READ_DEFAULT_LINES: usize = 2000;
/// Command output beyond this is clipped in the middle, keeping both ends.
const OUTPUT_MAX_CHARS: usize = 30_000;
const BASH_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const BASH_MAX_TIMEOUT: Duration = Duration::from_secs(600);
const LIST_MAX_ENTRIES: usize = 500;
const GREP_DEFAULT_MATCHES: usize = 200;
const GREP_MAX_MATCHES: usize = 2000;

/// The tools a loop round gets over `sandbox`, named as Claude Code names
/// them so the round's contract reads the same on every engine.
pub fn sandbox_tools(sandbox: Arc<dyn CodingSandbox>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Read(Arc::clone(&sandbox))),
        Arc::new(Write(Arc::clone(&sandbox))),
        Arc::new(Edit(Arc::clone(&sandbox))),
        Arc::new(Glob(Arc::clone(&sandbox))),
        Arc::new(Grep(Arc::clone(&sandbox))),
        Arc::new(Bash(sandbox)),
    ]
}

fn ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

fn invalid(e: serde_json::Error) -> eyre::Report {
    eyre::eyre!("Invalid input: {e}")
}

/// Keep both ends of a long text and say how much went missing.
fn clip(text: &str, max_chars: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= max_chars {
        return (text.to_string(), false);
    }
    let keep = max_chars / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(count - keep).collect();
    (
        format!(
            "{head}\n\n[… {} characters clipped …]\n\n{tail}",
            count - 2 * keep
        ),
        true,
    )
}

fn numbered(lines: &[&str], first: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{line}", first + i))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `find -path` pattern for a glob, anchored at the workspace root. `**/`
/// becomes `*`, which crosses directories under `-path`, so `**/models/*.rb`
/// finds `app/models/user.rb` and `models/user.rb` alike.
fn find_path_pattern(glob: &str) -> String {
    let stripped = glob.trim().trim_start_matches("./").replace("**/", "*");
    format!("./{stripped}")
}

/// The file's text, or the failure a tool reports for it.
async fn read_file(sandbox: &dyn CodingSandbox, path: &str) -> Result<String, String> {
    match sandbox.read(Path::new(path), FILE_MAX_BYTES).await {
        Ok(Some(content)) => Ok(content),
        Ok(None) => Err(format!("no file at '{path}'")),
        Err(e) => Err(e.to_string()),
    }
}

fn exec_error(out: &ExecOutcome) -> String {
    let detail = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    match (out.summary.timed_out, out.exit_code()) {
        (true, _) => format!("timed out\n{detail}"),
        (false, code) => format!("exit code {code}\n{detail}"),
    }
}

struct Read(Arc<dyn CodingSandbox>);

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    /// 1-based line to start from.
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file in the workspace, numbered by line. Use offset and limit for a large file."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace" },
                "offset": { "type": "integer", "description": "First line to return, 1-based (default 1)" },
                "limit": { "type": "integer", "description": "Lines to return (default 2000)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ReadInput = serde_json::from_value(input).map_err(invalid)?;
        let content = match read_file(self.0.as_ref(), &input.path).await {
            Ok(content) => content,
            Err(reason) => return Ok(ToolResult::failure(reason, ms(start))),
        };
        let lines: Vec<&str> = content.lines().collect();
        let first = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(READ_DEFAULT_LINES).max(1);
        let selected: Vec<&str> = lines.iter().skip(first - 1).take(limit).copied().collect();
        Ok(ToolResult::success(
            serde_json::json!({
                "path": input.path,
                "content": numbered(&selected, first),
                "lines": selected.len(),
                "total_lines": lines.len(),
                "truncated": content.len() >= FILE_MAX_BYTES,
            }),
            ms(start),
        ))
    }
}

struct Write(Arc<dyn CodingSandbox>);

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file in the workspace with the given content."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace" },
                "content": { "type": "string", "description": "The whole file" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: WriteInput = serde_json::from_value(input).map_err(invalid)?;
        match self.0.write(Path::new(&input.path), &input.content).await {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({ "path": input.path, "bytes": input.content.len() }),
                ms(start),
            )),
            Err(e) => Ok(ToolResult::failure(e.to_string(), ms(start))),
        }
    }
}

struct Edit(Arc<dyn CodingSandbox>);

#[derive(Deserialize)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. old_string must occur once unless replace_all is set."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path relative to the workspace" },
                "old_string": { "type": "string", "description": "Exact text to find" },
                "new_string": { "type": "string", "description": "Replacement text" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: EditInput = serde_json::from_value(input).map_err(invalid)?;
        if input.old_string.is_empty() {
            return Ok(ToolResult::failure("old_string is empty", ms(start)));
        }
        let path = Path::new(&input.path);
        let content = match read_file(self.0.as_ref(), &input.path).await {
            Ok(content) if content.len() >= FILE_MAX_BYTES => {
                return Ok(ToolResult::failure(
                    format!(
                        "'{}' is too large to edit ({FILE_MAX_BYTES} bytes or more)",
                        input.path
                    ),
                    ms(start),
                ))
            }
            Ok(content) => content,
            Err(reason) => return Ok(ToolResult::failure(reason, ms(start))),
        };
        let occurrences = content.matches(&input.old_string).count();
        let edited = match (occurrences, input.replace_all) {
            (0, _) => {
                return Ok(ToolResult::failure(
                    format!("old_string not found in '{}'", input.path),
                    ms(start),
                ))
            }
            (1, _) | (_, true) => content.replace(&input.old_string, &input.new_string),
            (n, false) => {
                return Ok(ToolResult::failure(
                    format!(
                    "old_string occurs {n} times in '{}'; include more context or set replace_all",
                    input.path
                ),
                    ms(start),
                ))
            }
        };
        match self.0.write(path, &edited).await {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({ "path": input.path, "replacements": occurrences }),
                ms(start),
            )),
            Err(e) => Ok(ToolResult::failure(e.to_string(), ms(start))),
        }
    }
}

struct Glob(Arc<dyn CodingSandbox>);

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by glob (e.g. `**/*.rb`, `spec/**/*_spec.rb`), sorted by path. Skips .git and node_modules."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern relative to path" },
                "path": { "type": "string", "description": "Directory to search (default: the workspace)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: GlobInput = serde_json::from_value(input).map_err(invalid)?;
        let script = "cd \"$1\" && find . \\( -name .git -o -name node_modules \\) -prune -o \
                      -type f -path \"$2\" -print | sort | head -n \"$3\"";
        let command = SandboxCommand::new(
            "sh",
            [
                "-c",
                script,
                "sh",
                &input.path.clone().unwrap_or_else(|| ".".into()),
                &find_path_pattern(&input.pattern),
                &(LIST_MAX_ENTRIES + 1).to_string(),
            ],
        );
        let out = match self.0.exec(command).await {
            Ok(out) => out,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), ms(start))),
        };
        if out.exit_code() != 0 {
            return Ok(ToolResult::failure(exec_error(&out), ms(start)));
        }
        let files: Vec<&str> = out
            .stdout
            .lines()
            .map(|line| line.strip_prefix("./").unwrap_or(line))
            .collect();
        Ok(ToolResult::success(
            serde_json::json!({
                "files": files.iter().take(LIST_MAX_ENTRIES).collect::<Vec<_>>(),
                "truncated": files.len() > LIST_MAX_ENTRIES,
            }),
            ms(start),
        ))
    }
}

struct Grep(Arc<dyn CodingSandbox>);

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with an extended regular expression; returns path:line:text matches. Skips .git and node_modules."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Extended regular expression" },
                "path": { "type": "string", "description": "File or directory to search (default: the workspace)" },
                "glob": { "type": "string", "description": "Only files matching this name glob, e.g. `*.rb`" },
                "max_results": { "type": "integer", "description": "Matches to return (default 200)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: GrepInput = serde_json::from_value(input).map_err(invalid)?;
        let max = input
            .max_results
            .unwrap_or(GREP_DEFAULT_MATCHES)
            .clamp(1, GREP_MAX_MATCHES);
        let script = "out=$(grep -rnIE --exclude-dir=.git --exclude-dir=node_modules \
                      ${3:+--include=\"$3\"} -e \"$1\" -- \"$2\"); rc=$?; \
                      [ -n \"$out\" ] && printf '%s\\n' \"$out\" | head -n \"$4\"; exit $rc";
        let command = SandboxCommand::new(
            "sh",
            [
                "-c",
                script,
                "sh",
                &input.pattern,
                &input.path.clone().unwrap_or_else(|| ".".into()),
                input.glob.as_deref().unwrap_or(""),
                &(max + 1).to_string(),
            ],
        );
        let out = match self.0.exec(command).await {
            Ok(out) => out,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), ms(start))),
        };
        match out.exit_code() {
            0 | 1 => {
                let matches: Vec<&str> = out
                    .stdout
                    .lines()
                    .map(|line| line.strip_prefix("./").unwrap_or(line))
                    .collect();
                Ok(ToolResult::success(
                    serde_json::json!({
                        "matches": matches.iter().take(max).collect::<Vec<_>>(),
                        "truncated": matches.len() > max,
                    }),
                    ms(start),
                ))
            }
            _ => Ok(ToolResult::failure(exec_error(&out), ms(start))),
        }
    }
}

struct Bash(Arc<dyn CodingSandbox>);

#[derive(Deserialize)]
struct BashInput {
    command: String,
    cwd: Option<PathBuf>,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the workspace and return its output and exit code. Long output is clipped in the middle."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line, run with sh -c" },
                "cwd": { "type": "string", "description": "Directory to run in, relative to the workspace" },
                "timeout_secs": { "type": "integer", "description": "Seconds before the command is killed (default 120, at most 600)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: BashInput = serde_json::from_value(input).map_err(invalid)?;
        let timeout = input
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(BASH_DEFAULT_TIMEOUT)
            .clamp(Duration::from_secs(1), BASH_MAX_TIMEOUT);
        let mut command =
            SandboxCommand::new("sh", ["-c", input.command.as_str()]).timeout(timeout);
        if let Some(cwd) = input.cwd {
            command = command.cwd(cwd);
        }
        let out = match self.0.exec(command).await {
            Ok(out) => out,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), ms(start))),
        };
        let (stdout, clipped_out) = clip(&out.stdout, OUTPUT_MAX_CHARS);
        let (stderr, clipped_err) = clip(&out.stderr, OUTPUT_MAX_CHARS);
        if out.summary.timed_out || out.exit_code() != 0 {
            let cut = ExecOutcome {
                stdout,
                stderr,
                summary: out.summary,
            };
            return Ok(ToolResult::failure(exec_error(&cut), ms(start)));
        }
        Ok(ToolResult::success(
            serde_json::json!({
                "exit_code": 0,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": clipped_out || clipped_err || out.summary.truncated,
            }),
            ms(start),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding_sandbox::adopt_workdir;

    fn workspace() -> (tempfile::TempDir, Vec<Arc<dyn Tool>>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("spec/models")).unwrap();
        std::fs::write(
            dir.path().join("app.rb"),
            "class App\n  def go\n  end\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("spec/models/app_spec.rb"),
            "describe App do\n  it 'goes' do\n  end\nend\n",
        )
        .unwrap();
        let tools = sandbox_tools(adopt_workdir(dir.path().to_path_buf(), Vec::new(), None));
        (dir, tools)
    }

    fn tool<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
        tools.iter().find(|t| t.name() == name).unwrap()
    }

    #[test]
    fn the_six_base_tools_carry_claude_codes_names() {
        let (_dir, tools) = workspace();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]);
    }

    #[test]
    fn globs_anchor_at_the_root_and_let_star_cross_directories() {
        assert_eq!(find_path_pattern("**/*.rb"), "./**.rb");
        assert_eq!(find_path_pattern("spec/**/*_spec.rb"), "./spec/**_spec.rb");
        assert_eq!(find_path_pattern("**/models/*.rb"), "./*models/*.rb");
        assert_eq!(find_path_pattern("./lib/*.rs"), "./lib/*.rs");
    }

    #[test]
    fn clipping_keeps_both_ends() {
        let (kept, clipped) = clip(&"x".repeat(100), 10);
        assert!(clipped);
        assert!(kept.starts_with("xxxxx\n") && kept.ends_with("\nxxxxx"));
        assert!(kept.contains("90 characters clipped"));
        assert_eq!(clip("short", 10), ("short".to_string(), false));
    }

    #[tokio::test]
    async fn read_numbers_lines_and_windows_them() {
        let (_dir, tools) = workspace();
        let read = tool(&tools, "Read");
        let all = read
            .execute(serde_json::json!({ "path": "app.rb" }))
            .await
            .unwrap();
        assert!(all.success);
        assert_eq!(all.output["total_lines"], 4);
        assert!(all.output["content"]
            .as_str()
            .unwrap()
            .starts_with("     1\tclass App\n     2\t  def go"));
        let window = read
            .execute(serde_json::json!({ "path": "app.rb", "offset": 2, "limit": 1 }))
            .await
            .unwrap();
        assert_eq!(window.output["content"], "     2\t  def go");
        let missing = read
            .execute(serde_json::json!({ "path": "nope.rb" }))
            .await
            .unwrap();
        assert!(!missing.success);
        assert!(missing.error.unwrap().contains("no file"));
        let outside = read
            .execute(serde_json::json!({ "path": "../etc/passwd" }))
            .await
            .unwrap();
        assert!(!outside.success, "the sandbox holds the boundary");
    }

    #[tokio::test]
    async fn write_then_edit_requires_a_unique_match_unless_told_otherwise() {
        let (dir, tools) = workspace();
        let write = tool(&tools, "Write");
        let edit = tool(&tools, "Edit");
        assert!(
            write
                .execute(serde_json::json!({ "path": "lib/new.rb", "content": "a = 1\nb = 1\n" }))
                .await
                .unwrap()
                .success
        );
        let ambiguous = edit
            .execute(serde_json::json!({
                "path": "lib/new.rb", "old_string": "= 1", "new_string": "= 2"
            }))
            .await
            .unwrap();
        assert!(!ambiguous.success);
        assert!(ambiguous.error.unwrap().contains("occurs 2 times"));
        let all = edit
            .execute(serde_json::json!({
                "path": "lib/new.rb", "old_string": "= 1", "new_string": "= 2", "replace_all": true
            }))
            .await
            .unwrap();
        assert!(all.success);
        assert_eq!(all.output["replacements"], 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("lib/new.rb")).unwrap(),
            "a = 2\nb = 2\n"
        );
        let absent = edit
            .execute(serde_json::json!({
                "path": "lib/new.rb", "old_string": "zzz", "new_string": ""
            }))
            .await
            .unwrap();
        assert!(absent.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn edit_refuses_a_file_it_could_only_read_in_part() {
        let (dir, tools) = workspace();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(FILE_MAX_BYTES)).unwrap();
        let refused = tool(&tools, "Edit")
            .execute(serde_json::json!({
                "path": "big.txt", "old_string": "x", "new_string": "y", "replace_all": true
            }))
            .await
            .unwrap();
        assert!(!refused.success);
        assert!(refused.error.unwrap().contains("too large"));
        assert_eq!(
            std::fs::metadata(dir.path().join("big.txt")).unwrap().len() as usize,
            FILE_MAX_BYTES,
            "nothing was written back"
        );
    }

    #[tokio::test]
    async fn glob_and_grep_find_files_and_lines() {
        let (_dir, tools) = workspace();
        let found = tool(&tools, "Glob")
            .execute(serde_json::json!({ "pattern": "**/*_spec.rb" }))
            .await
            .unwrap();
        assert!(found.success, "{:?}", found.error);
        assert_eq!(
            found.output["files"],
            serde_json::json!(["spec/models/app_spec.rb"])
        );
        let nested = tool(&tools, "Glob")
            .execute(serde_json::json!({ "pattern": "**/models/*.rb" }))
            .await
            .unwrap();
        assert_eq!(
            nested.output["files"],
            serde_json::json!(["spec/models/app_spec.rb"]),
            "`**/` reaches a directory at any depth"
        );
        let hits = tool(&tools, "Grep")
            .execute(serde_json::json!({ "pattern": "def go|it 'goes'", "glob": "*.rb" }))
            .await
            .unwrap();
        assert!(hits.success, "{:?}", hits.error);
        let matches = hits.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2, "{matches:?}");
        assert!(matches
            .iter()
            .any(|m| m.as_str().unwrap() == "app.rb:2:  def go"));
        let none = tool(&tools, "Grep")
            .execute(serde_json::json!({ "pattern": "unicorn" }))
            .await
            .unwrap();
        assert!(none.success);
        assert_eq!(none.output["matches"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn bash_reports_output_exit_codes_and_timeouts() {
        let (_dir, tools) = workspace();
        let bash = tool(&tools, "Bash");
        let ok = bash
            .execute(serde_json::json!({ "command": "printf hello; printf err >&2" }))
            .await
            .unwrap();
        assert!(ok.success);
        assert_eq!(ok.output["stdout"], "hello");
        assert_eq!(ok.output["stderr"], "err");
        let failed = bash
            .execute(serde_json::json!({ "command": "echo boom >&2; exit 3", "cwd": "spec" }))
            .await
            .unwrap();
        assert!(!failed.success);
        assert_eq!(failed.error.as_deref(), Some("exit code 3\nboom"));
        let slow = bash
            .execute(serde_json::json!({ "command": "sleep 5", "timeout_secs": 1 }))
            .await
            .unwrap();
        assert!(!slow.success);
        assert!(slow.error.unwrap().starts_with("timed out"));
    }
}
