//! code.refactor - structural edits: replace a definition by name, rename a
//! symbol everywhere it is used, and ask a language server for a file's
//! diagnostics, a symbol's references or its definition
//!.
//!
//! Replacing a definition parses the file with tree-sitter and needs nothing
//! running. The rest is answered by a language server for the file's
//! project, started on demand through the executor and reused across calls.
//! Every write is confined to the agent's roots, made whole or not at all,
//! and refused when a file changed since the edit was computed.

pub mod apply;
pub mod lsp;

pub use crate::hub_core::capabilities::runs_language_server;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::paths::resolve_real;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_codebase_knowledge::definitions::{self, Grammar};
use workforce_common::settings::RefactorConfig;

use self::apply::{open_regular, read_regular, FileChange};
use self::lsp::edit::{apply_edits, document_edits, offset as offset_of, Encoding};
use self::lsp::{
    path_of, Document, LanguageServer, Launcher, LspError, Pool, ProcessLauncher, ServerKind,
    ServerSpec, Timeouts,
};
use super::compile_gate::{cargo_workspace, nearest_with, repo_root};
use super::utils::validate_path;

pub const TOOL_NAME: &str = "code.refactor";

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
/// Largest document shown to a language server, and the most a rename may
/// show it again to bring its view up to date: what it is sent waits in the
/// executor's bounded input backlog while the server is busy.
const MAX_SERVER_DOCUMENT: usize = 2 * 1024 * 1024;
const MAX_REFRESHED: usize = 4 * MAX_SERVER_DOCUMENT;
/// Lines listed for a symbol that appears on several.
const MAX_LINES_LISTED: usize = 20;
/// Diagnostics, references or definitions listed at most.
const MAX_LISTED: usize = 100;
/// Characters of one diagnostic's message shown.
const MAX_MESSAGE: usize = 500;

/// The agent's `[tools.refactor]` as of this call; `None` when it has none.
pub type LiveConfig = Arc<dyn Fn() -> Option<RefactorConfig> + Send + Sync>;

pub struct CodeRefactorTool {
    roots: Vec<PathBuf>,
    config: LiveConfig,
    launcher: Arc<dyn Launcher>,
    pool: Arc<Pool>,
}

impl CodeRefactorTool {
    /// The tool over the hub's shared servers, started through `runner` with
    /// the install's limits.
    pub fn new(
        roots: Vec<PathBuf>,
        config: LiveConfig,
        install: &RefactorConfig,
        runner: Option<RunnerEndpoint>,
    ) -> Self {
        let launcher = Arc::new(ProcessLauncher {
            runner,
            lifetime: Duration::from_secs(install.max_lifetime_secs),
            max_rss_bytes: install.max_memory_mb << 20,
        });
        let pool = Pool::shared(install.max_servers, Duration::from_secs(install.idle_secs));
        Self::with_parts(roots, config, launcher, pool)
    }

    pub fn with_parts(
        roots: Vec<PathBuf>,
        config: LiveConfig,
        launcher: Arc<dyn Launcher>,
        pool: Arc<Pool>,
    ) -> Self {
        Self {
            roots,
            config,
            launcher,
            pool,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    ReplaceDefinition,
    Rename,
    Diagnostics,
    References,
    Definition,
}

#[derive(Debug, Deserialize)]
struct Input {
    action: Action,
    path: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    line: Option<usize>,
    #[serde(default)]
    new_content: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
}

/// A call's failure, as the model is told it.
#[derive(Debug)]
struct Refusal(String);

impl<T: std::fmt::Display> From<T> for Refusal {
    fn from(reason: T) -> Self {
        Self(reason.to_string())
    }
}

type Outcome = std::result::Result<Value, Refusal>;

fn required<'a>(value: &'a Option<String>, name: &str, action: &str) -> Result<&'a str, Refusal> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Refusal(format!("{name} is required for {action}")))
}

/// What a file that a language server cannot read is written in.
const LANGUAGES: &str =
    "Rust (.rs) and TypeScript or JavaScript (.ts, .tsx, .mts, .cts, .js, .jsx, .mjs, .cjs)";

impl CodeRefactorTool {
    fn read(&self, raw: &str) -> Result<Document, Refusal> {
        let path = validate_path(Path::new(raw), &self.roots)?;
        let mut file = open_regular(&path, false).map_err(|e| format!("{raw}: {e}"))?;
        let size = file.metadata().map_err(|e| format!("{raw}: {e}"))?.len();
        if size > MAX_FILE_SIZE {
            return Err(Refusal(format!(
                "{raw} is {size} bytes, over the {MAX_FILE_SIZE}-byte limit"
            )));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| format!("{raw}: {e}"))?;
        Ok(Document { path, text })
    }

    async fn run(&self, input: Input, config: RefactorConfig) -> Outcome {
        let document = self.read(&input.path)?;
        let grammar = Grammar::of_path(&document.path).ok_or_else(|| {
            Refusal(format!(
                "code.refactor reads {LANGUAGES}; for {} use code.edit or code.find_replace",
                input.path
            ))
        })?;
        match input.action {
            Action::ReplaceDefinition => replace_definition(&input, document, grammar),
            _ if !config.language_servers => Err(Refusal(
                "language-server actions are turned off for this agent; replace_definition still works".into(),
            )),
            action => self.ask_server(action, &input, document, grammar, &config).await,
        }
    }

    async fn ask_server(
        &self,
        action: Action,
        input: &Input,
        document: Document,
        grammar: Grammar,
        config: &RefactorConfig,
    ) -> Outcome {
        if document.text.len() > MAX_SERVER_DOCUMENT {
            return Err(Refusal(format!(
                "{} is over the {MAX_SERVER_DOCUMENT}-byte limit for a language server; replace_definition and code.edit still work",
                input.path
            )));
        }
        let kind = ServerKind::of(grammar);
        let project = self.project(&document.path, kind)?;
        let spec = ServerSpec {
            kind,
            root: project.root.clone(),
            command: match kind {
                ServerKind::RustAnalyzer => config.rust_analyzer.clone(),
                ServerKind::TypeScript => config.typescript.clone(),
            },
        };
        let timeouts = Timeouts {
            startup: Duration::from_secs(config.startup_timeout_secs),
            request: Duration::from_secs(config.request_timeout_secs),
        };
        let mut lease = self
            .pool
            .checkout(spec, self.launcher.as_ref(), timeouts, config.max_memory_mb)
            .await?;
        let server = lease.server();
        let name = server.spec.kind.name();
        let asked = match action {
            Action::Rename => {
                self.rename(server, timeouts, input, &document, grammar, &project)
                    .await
            }
            Action::Diagnostics => server
                .diagnostics(timeouts, &document)
                .await
                .map(|published| Ok(diagnostics_output(&document, published, timeouts, name))),
            Action::References | Action::Definition => {
                let symbol = required(&input.symbol, "symbol", "this action")?;
                let at = locate(&document.text, grammar, symbol, input.line)?;
                let found = match action {
                    Action::References => server.references(timeouts, &document, at).await,
                    _ => server.definition(timeouts, &document, at).await,
                };
                found.map(|found| {
                    Ok(self.locations_output(action, symbol, found, server.encoding(), &project))
                })
            }
            Action::ReplaceDefinition => unreachable!("answered without a server"),
        };
        match asked {
            Ok(outcome) => outcome,
            Err(e @ LspError::Broken(_)) => {
                lease.discard();
                Err(e.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The project the file's language server is started for: its Cargo
    /// workspace, or its nearest TypeScript or JavaScript project, never
    /// above its checkout or the agent's root.
    fn project(&self, file: &Path, kind: ServerKind) -> Result<Project, Refusal> {
        let root = self
            .roots
            .iter()
            .map(|root| resolve_real(root))
            .filter(|root| file.starts_with(root))
            .max_by_key(|root| root.components().count())
            .ok_or_else(|| Refusal(format!("{} is outside the workspace", file.display())))?;
        let checkout = repo_root(file, &root).unwrap_or(root);
        let root = match kind {
            ServerKind::RustAnalyzer => nearest_with(file, &checkout, "Cargo.toml")
                .map(|package| cargo_workspace(&package, &checkout))
                .ok_or_else(|| {
                    Refusal(format!(
                        "{} is in no Cargo package, so rust-analyzer cannot load it",
                        file.display()
                    ))
                })?,
            ServerKind::TypeScript => ["tsconfig.json", "jsconfig.json", "package.json"]
                .iter()
                .find_map(|marker| nearest_with(file, &checkout, marker))
                .unwrap_or_else(|| checkout.clone()),
        };
        Ok(Project {
            kind,
            root,
            checkout,
        })
    }

    /// The file a language server named, when the hub may read and write it
    /// on the server's word. A server runs the project's own code, and the
    /// hub's user opens what the agent's cannot, so only source in the
    /// server's language, inside the checkout it was started for and in no
    /// directory closed to the agent's user, is ever touched for it.
    fn named_by_server(&self, uri: &str, project: &Project) -> Option<PathBuf> {
        let path = validate_path(&path_of(uri)?, &self.roots).ok()?;
        let source = Grammar::of_path(&path).map(ServerKind::of) == Some(project.kind);
        (source && path.starts_with(&project.checkout) && !in_closed_directory(&path, project))
            .then_some(path)
    }

    /// Rename, apply the edit, and once retry a rename the server computed
    /// from a stale view of some file, after showing it the file as it is.
    async fn rename(
        &self,
        server: &mut LanguageServer,
        timeouts: Timeouts,
        input: &Input,
        document: &Document,
        grammar: Grammar,
        project: &Project,
    ) -> std::result::Result<Outcome, LspError> {
        let (symbol, new_name) = match (
            required(&input.symbol, "symbol", "rename"),
            required(&input.new_name, "new_name", "rename"),
        ) {
            (Ok(symbol), Ok(new_name)) => (symbol, new_name.trim()),
            (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
        };
        if new_name.chars().any(char::is_whitespace) {
            return Ok(Err(Refusal(format!("`{new_name}` is not a name"))));
        }
        let at = match locate(&document.text, grammar, symbol, input.line) {
            Ok(at) => at,
            Err(e) => return Ok(Err(e)),
        };
        let old = identifier_at(&document.text, at).to_string();
        if old.is_empty() {
            return Ok(Err(Refusal(format!("`{symbol}` does not name a symbol"))));
        }
        let mut refreshed = false;
        loop {
            let (edit, version) = server.rename(timeouts, document, at, new_name).await?;
            let asked = Asked {
                opened: document,
                version,
                old: &old,
            };
            match self.changes(&edit, server.encoding(), &asked, project) {
                Ok(changes) => return Ok(write_rename(&old, new_name, changes)),
                Err(Stale(stale)) if !refreshed => {
                    server.refresh(&stale).await?;
                    refreshed = true;
                }
                Err(Stale(stale)) => {
                    return Ok(Err(Refusal(format!(
                        "{} still sees an older version of {}; nothing was changed",
                        server.spec.kind.name(),
                        stale
                            .iter()
                            .map(|d| d.path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))))
                }
                Err(Rejected(why)) => {
                    return Ok(Err(Refusal(format!("{why}; nothing was changed"))))
                }
            }
        }
    }

    /// The files a rename's `WorkspaceEdit` writes, each confined to the
    /// server's project and held to the text the server was given.
    fn changes(
        &self,
        edit: &Value,
        encoding: Encoding,
        asked: &Asked<'_>,
        project: &Project,
    ) -> std::result::Result<Vec<(FileChange, usize)>, RenameProblem> {
        let documents = document_edits(edit).map_err(Rejected)?;
        if documents.is_empty() {
            return Err(Rejected(
                "the language server found nothing to rename".into(),
            ));
        }
        let mut changes = Vec::with_capacity(documents.len());
        let mut stale = Vec::new();
        for edits in documents {
            let path = self.named_by_server(&edits.uri, project).ok_or_else(|| {
                Rejected(format!(
                    "the rename would edit {}, which is not source in {}",
                    edits.uri,
                    project.checkout.display()
                ))
            })?;
            let before =
                read_regular(&path).map_err(|e| Rejected(format!("{}: {e}", path.display())))?;
            let is_opened = path == asked.opened.path;
            if is_opened
                && (before != asked.opened.text
                    || edits.version.is_some_and(|v| v != asked.version))
            {
                return Err(Rejected(format!(
                    "{} changed while it was being renamed in",
                    path.display()
                )));
            }
            match apply_edits(&before, &edits.edits, encoding, |span| {
                same_name(span, asked.old)
            }) {
                Ok(after) => changes.push((
                    FileChange {
                        path,
                        before,
                        after,
                    },
                    edits.edits.len(),
                )),
                Err(_) if !is_opened => stale.push(Document { path, text: before }),
                Err(why) => return Err(Rejected(why)),
            }
        }
        if stale.is_empty() {
            return Ok(changes);
        }
        let size: usize = stale.iter().map(|document| document.text.len()).sum();
        match size > MAX_REFRESHED {
            true => Err(Rejected(format!(
                "{} sees an older version of {} files too large to show it again",
                project.kind.name(),
                stale.len()
            ))),
            false => Err(Stale(stale)),
        }
    }

    fn locations_output(
        &self,
        action: Action,
        symbol: &str,
        found: Vec<Value>,
        encoding: Encoding,
        project: &Project,
    ) -> Value {
        let mut texts: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
        let mut outside = 0usize;
        let listed: Vec<Value> = found
            .iter()
            .filter_map(|location| {
                let inside = location
                    .get("uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| self.named_by_server(uri, project));
                let Some(path) = inside else {
                    outside += 1;
                    return None;
                };
                let text = texts
                    .entry(path.clone())
                    .or_insert_with(|| read_regular(&path).ok())
                    .as_deref()?;
                let start = location.pointer("/range/start")?;
                let at = offset_of(text, start, encoding)?;
                let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
                let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
                Some(json!({
                    "path": path.display().to_string(),
                    "line": text[..at].matches('\n').count() + 1,
                    "column": text[line_start..at].chars().count() + 1,
                    "text": text[line_start..line_end].trim(),
                }))
            })
            .collect();
        let key = match action {
            Action::References => "references",
            _ => "definitions",
        };
        let mut output = json!({
            "action": key.trim_end_matches('s'),
            "symbol": symbol,
            "truncated": listed.len() > MAX_LISTED,
        });
        output[key] = Value::Array(listed.into_iter().take(MAX_LISTED).collect());
        if outside > 0 {
            output["outside_workspace"] = json!(outside);
        }
        output
    }
}

/// Where a language server runs and what it may name.
struct Project {
    kind: ServerKind,
    /// The directory the server is started in.
    root: PathBuf,
    /// The git checkout around it, or the agent's root when there is none.
    checkout: PathBuf,
}

/// Whether a directory between `file` and its checkout is closed to all but
/// its owner, as the hub's private state is.
#[cfg(unix)]
fn in_closed_directory(file: &Path, project: &Project) -> bool {
    use std::os::unix::fs::PermissionsExt;
    file.ancestors()
        .skip(1)
        .take_while(|dir| *dir != project.checkout && dir.starts_with(&project.checkout))
        .any(|dir| std::fs::metadata(dir).is_ok_and(|meta| meta.permissions().mode() & 0o077 == 0))
}

#[cfg(not(unix))]
fn in_closed_directory(_file: &Path, _project: &Project) -> bool {
    false
}

/// What a rename was asked about: the document opened for it, the version
/// it was opened at, and the name being replaced.
struct Asked<'a> {
    opened: &'a Document,
    version: i64,
    old: &'a str,
}

use RenameProblem::{Rejected, Stale};

enum RenameProblem {
    /// Files the server's view of was out of date.
    Stale(Vec<Document>),
    Rejected(String),
}

/// Write a rename's files, and say what changed.
fn write_rename(old: &str, new_name: &str, changes: Vec<(FileChange, usize)>) -> Outcome {
    let (files, counts): (Vec<FileChange>, Vec<usize>) = changes.into_iter().unzip();
    apply::apply(&files)?;
    Ok(json!({
        "action": "rename",
        "symbol": old,
        "new_name": new_name,
        "edits": counts.iter().sum::<usize>(),
        "files": files
            .iter()
            .zip(&counts)
            .map(|(file, edits)| json!({"path": file.path.display().to_string(), "edits": edits}))
            .collect::<Vec<_>>(),
    }))
}

/// Whether a span a rename replaces still reads as the old name: quoted, as
/// a string-keyed property is, or raw, as a Rust keyword-named item is.
fn same_name(span: &str, old: &str) -> bool {
    let bare = |name: &str| {
        name.trim_matches(['"', '\'', '`'])
            .trim_start_matches("r#")
            .to_string()
    };
    bare(span) == bare(old)
}

fn replace_definition(input: &Input, document: Document, grammar: Grammar) -> Outcome {
    let symbol = required(&input.symbol, "symbol", "replace_definition")?;
    let new_content = input
        .new_content
        .as_deref()
        .ok_or_else(|| Refusal("new_content is required for replace_definition".into()))?;
    let replaced = definitions::replace(&document.text, grammar, symbol, input.line, new_content)?;
    let path = document.path.display().to_string();
    apply::apply(&[FileChange {
        path: document.path,
        before: document.text,
        after: replaced.source,
    }])?;
    Ok(json!({
        "action": "replace_definition",
        "symbol": replaced.label,
        "kind": replaced.kind,
        "lines": [replaced.start_line, replaced.end_line],
        "lines_removed": replaced.lines_removed,
        "files": [{"path": path, "edits": 1}],
    }))
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '#'
}

/// The identifier the query ends in: `new` for `Config::new`.
fn identifier(symbol: &str) -> &str {
    symbol
        .rsplit("::")
        .next()
        .and_then(|last| last.rsplit('.').next())
        .unwrap_or(symbol)
        .trim()
}

fn identifier_at(text: &str, at: usize) -> &str {
    let end = text[at..]
        .find(|c: char| !is_identifier_char(c))
        .map_or(text.len(), |i| at + i);
    &text[at..end]
}

/// Where in `text` the symbol a call names is: its occurrence on `line`
/// when one is given, else its definition in the file, else its only
/// occurrence.
fn locate(
    text: &str,
    grammar: Grammar,
    symbol: &str,
    line: Option<usize>,
) -> Result<usize, Refusal> {
    let name = identifier(symbol);
    if name.is_empty() || symbol.trim_start().starts_with("impl ") {
        return Err(Refusal(format!("`{symbol}` does not name a symbol")));
    }
    let occurrences: Vec<usize> = text
        .match_indices(name)
        .map(|(at, _)| at)
        .filter(|&at| {
            !text[..at]
                .chars()
                .next_back()
                .is_some_and(is_identifier_char)
                && !text[at + name.len()..]
                    .chars()
                    .next()
                    .is_some_and(is_identifier_char)
        })
        .collect();
    let line_of = |at: usize| text[..at].matches('\n').count() + 1;
    if let Some(line) = line {
        return occurrences
            .iter()
            .copied()
            .find(|&at| line_of(at) == line)
            .ok_or_else(|| Refusal(format!("`{name}` does not appear on line {line}")));
    }
    if let Ok(definition) = definitions::find_one(text, grammar, symbol, None) {
        if definition.kind != "impl" {
            return Ok(definition.name_offset);
        }
    }
    match occurrences.as_slice() {
        [only] => Ok(*only),
        [] => Err(Refusal(format!("`{name}` does not appear in the file"))),
        many => Err(Refusal(format!(
            "`{name}` appears {} times, on lines {}{}; pass the line of the one you mean",
            many.len(),
            many.iter()
                .take(MAX_LINES_LISTED)
                .map(|&at| line_of(at).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            match many.len() > MAX_LINES_LISTED {
                true => " and more",
                false => "",
            }
        ))),
    }
}

fn diagnostics_output(
    document: &Document,
    published: Option<lsp::rpc::Published>,
    timeouts: Timeouts,
    server: &str,
) -> Value {
    let Some(published) = published else {
        return json!({
            "action": "diagnostics",
            "path": document.path.display().to_string(),
            "server": server,
            "complete": false,
            "diagnostics": [],
            "note": format!("{server} published no diagnostics within {}s", timeouts.request.as_secs()),
        });
    };
    let severity = |level: Option<i64>| match level {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "information",
        _ => "hint",
    };
    let listed: Vec<Value> = published
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let start = diagnostic.pointer("/range/start");
            let message: String = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(MAX_MESSAGE)
                .collect();
            json!({
                "line": start.and_then(|s| s.get("line")).and_then(Value::as_u64).map_or(0, |l| l + 1),
                "column": start.and_then(|s| s.get("character")).and_then(Value::as_u64).map_or(0, |c| c + 1),
                "severity": severity(diagnostic.get("severity").and_then(Value::as_i64)),
                "message": message,
                "code": diagnostic.get("code").cloned().unwrap_or(Value::Null),
                "source": diagnostic.get("source").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({
        "action": "diagnostics",
        "path": document.path.display().to_string(),
        "server": server,
        "complete": true,
        "truncated": listed.len() > MAX_LISTED,
        "diagnostics": listed.into_iter().take(MAX_LISTED).collect::<Vec<_>>(),
    })
}

#[async_trait]
impl Tool for CodeRefactorTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Structural code edits; prefer it to code.edit for renames and for rewriting a whole \
         function, method, struct, class or impl. replace_definition swaps the named definition \
         for new_content (its doc comments, attributes and decorators stay unless new_content \
         brings its own) and refuses a result that does not parse. rename renames a symbol \
         everywhere it is used, through the project's language server. diagnostics lists a \
         file's errors and warnings (Rust: rust-analyzer's own, not `cargo check`); references \
         and definition find a symbol's uses and where it is defined. Rust, TypeScript and \
         JavaScript files only."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["replace_definition", "rename", "diagnostics", "references", "definition"],
                },
                "path": {
                    "type": "string",
                    "description": "The file (relative to the workspace or absolute)"
                },
                "symbol": {
                    "type": "string",
                    "description": "What to act on. replace_definition: the definition's name, qualified when needed (`parse`, `Config::new`, `impl Display for Config`, `Widget.render`). rename/references/definition: the symbol as written in the file"
                },
                "line": {
                    "type": "integer",
                    "description": "1-based line where the symbol appears, to pick one of several"
                },
                "new_content": {
                    "type": "string",
                    "description": "replace_definition: the whole new definition, from its visibility or export keyword on"
                },
                "new_name": {
                    "type": "string",
                    "description": "rename: the new name"
                }
            },
            "required": ["action", "path"]
        })
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("?");
        Some(match input.get("action").and_then(Value::as_str)? {
            "replace_definition" => format!("replace `{}` in {}", text("symbol"), text("path")),
            "rename" => format!(
                "rename `{}` to `{}` from {}",
                text("symbol"),
                text("new_name"),
                text("path")
            ),
            action => format!("{action} for {}", text("path")),
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let Some(config) = (self.config)() else {
            return Ok(ToolResult::failure(
                "code.refactor is not enabled for this agent; use code.edit",
                elapsed(),
            ));
        };
        let input: Input = match serde_json::from_value(input) {
            Ok(input) => input,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };
        Ok(match self.run(input, config).await {
            Ok(output) => ToolResult::success(output, elapsed()),
            Err(Refusal(reason)) => ToolResult::failure(reason, elapsed()),
        })
    }
}

#[cfg(test)]
mod tests;
