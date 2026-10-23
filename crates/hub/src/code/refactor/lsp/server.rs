//! One language server for one project: started through the executor,
//! initialised, waited on until it has loaded the project, and asked one
//! question at a time about documents opened for that question only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::AbortOnDropHandle;
use workforce_agent::tools::exec_runner::{
    spawn_interactive, ExecRequest, InteractiveRequest, RunnerEndpoint, ServerFrame,
};
use workforce_codebase_knowledge::definitions::Grammar;

use super::edit::{position, Encoding};
use super::rpc::{Published, Rpc, RpcError};

/// The language servers `code.refactor` speaks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerKind {
    RustAnalyzer,
    TypeScript,
}

impl ServerKind {
    /// The server that reads files of `grammar`.
    pub fn of(grammar: Grammar) -> Self {
        match grammar {
            Grammar::Rust => Self::RustAnalyzer,
            _ => Self::TypeScript,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::TypeScript => "typescript-language-server",
        }
    }

    /// Settings sent at start: nothing that runs `cargo check` on its own,
    /// no background indexing beyond what a question needs, and the server
    /// watching the files itself, since other tools change them.
    fn initialization_options(self, max_memory_mb: u64) -> Value {
        match self {
            Self::RustAnalyzer => json!({
                "checkOnSave": false,
                "cachePriming": {"enable": false},
                "files": {"watcher": "server"},
                "cargo": {"buildScripts": {"enable": true}},
                "procMacro": {"enable": true},
            }),
            // tsserver's heap stays under the limit its process group is
            // held to, which also counts the language server in front of it.
            Self::TypeScript => json!({
                "maxTsServerMemory": max_memory_mb * 3 / 4,
                "tsserver": {"useSyntaxServer": "never"},
                "preferences": {"providePrefixAndSuffixTextForRename": true},
            }),
        }
    }

    /// What the server's environment adds. rustup, asked for a toolchain a
    /// repo pins and the hub lacks, would otherwise download it unasked.
    fn env(self) -> Vec<(String, String)> {
        match self {
            Self::RustAnalyzer => vec![("RUSTUP_AUTO_INSTALL".into(), "0".into())],
            Self::TypeScript => Vec::new(),
        }
    }

    /// Whether the server says when it has finished loading the project.
    fn reports_quiescence(self) -> bool {
        matches!(self, Self::RustAnalyzer)
    }
}

/// The language a document is opened as.
fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        _ => "typescript",
    }
}

/// Which server, for which project, started how.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerSpec {
    pub kind: ServerKind,
    pub root: PathBuf,
    pub command: Vec<String>,
}

/// A started server process: its stdin and stdout, and why it ended once
/// it has.
pub struct Launched {
    pub stdin: mpsc::Sender<String>,
    pub stdout: mpsc::Receiver<String>,
    pub ended: oneshot::Receiver<String>,
    /// Dropped, the process ends.
    pub process: Option<AbortOnDropHandle<()>>,
}

/// Starts server processes.
#[async_trait]
pub trait Launcher: Send + Sync {
    async fn launch(&self, spec: &ServerSpec) -> Result<Launched, String>;
}

/// Starts each server as `exec.run` and the `code.*` runners run commands:
/// through the privilege-separated executor when one is configured, as the
/// agent's user with a scrubbed environment, and otherwise in the hub
/// process without the hub's secrets.
pub struct ProcessLauncher {
    pub runner: Option<RunnerEndpoint>,
    pub lifetime: Duration,
    pub max_rss_bytes: u64,
}

/// Bytes of a server's stderr kept to explain why it failed.
const STDERR_TAIL: usize = 2048;

/// Memory left free on the host beyond a new server's own limit.
const MEMORY_HEADROOM_MB: u64 = 1024;

/// What the host and the hub's own container say about memory.
#[derive(Default)]
struct MemoryReport {
    /// `/proc/meminfo`.
    meminfo: String,
    /// The container's `memory.max`, `memory.current` and `memory.stat`.
    limit: String,
    used: String,
    stat: String,
}

impl MemoryReport {
    fn read() -> Self {
        let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
        Self {
            meminfo: read("/proc/meminfo"),
            limit: read("/sys/fs/cgroup/memory.max"),
            used: read("/sys/fs/cgroup/memory.current"),
            stat: read("/sys/fs/cgroup/memory.stat"),
        }
    }

    /// MiB a new process can take: what the host has available, or what is
    /// left under the container's limit when that is less. The file cache
    /// counts against neither, since it is given up under pressure. `None`
    /// where neither is reported.
    fn available_mb(&self) -> Option<u64> {
        let field = |text: &str, name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        };
        let host = field(&self.meminfo, "MemAvailable:").map(|kib| kib >> 10);
        let container = self.limit.trim().parse::<u64>().ok().and_then(|limit| {
            let used = self.used.trim().parse::<u64>().ok()?;
            let cache = field(&self.stat, "file ").unwrap_or(0);
            Some(limit.saturating_sub(used.saturating_sub(cache)) >> 20)
        });
        match (host, container) {
            (Some(host), Some(container)) => Some(host.min(container)),
            (host, container) => host.or(container),
        }
    }
}

/// Refuse to start a server its host has no room for: `limit_mb` is what it
/// may grow to before it is stopped, so that much must be free now. `None`
/// where the host does not say what it has free.
fn admit(available_mb: Option<u64>, limit_mb: u64, name: &str) -> Result<(), String> {
    match available_mb {
        Some(free) if free < limit_mb + MEMORY_HEADROOM_MB => Err(format!(
            "{name} is not started: it may use {limit_mb} MiB and the hub has {free} MiB available; replace_definition and code.edit still work"
        )),
        _ => Ok(()),
    }
}

impl ProcessLauncher {
    fn request(&self, spec: &ServerSpec) -> Result<InteractiveRequest, String> {
        let (program, args) = spec
            .command
            .split_first()
            .ok_or_else(|| format!("no command is configured for {}", spec.kind.name()))?;
        Ok(InteractiveRequest {
            exec: ExecRequest {
                program: program.clone(),
                args: args.to_vec(),
                workdir: Some(spec.root.display().to_string()),
                timeout_secs: self.lifetime.as_secs(),
                allow_retry: false,
                env: spec.kind.env(),
                stdin: None,
                scope: None,
                mcp: None,
            },
            max_rss_bytes: Some(self.max_rss_bytes),
        })
    }
}

#[async_trait]
impl Launcher for ProcessLauncher {
    async fn launch(&self, spec: &ServerSpec) -> Result<Launched, String> {
        let request = self.request(spec)?;
        admit(
            MemoryReport::read().available_mb(),
            self.max_rss_bytes >> 20,
            spec.kind.name(),
        )?;
        let running =
            spawn_interactive(self.runner.as_ref(), request, crate::code::utils::CODE_ENV);
        let (stdout_tx, stdout) = mpsc::channel(64);
        let (ended_tx, ended) = oneshot::channel();
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut output = running.output;
        let tail = Arc::clone(&stderr);
        let done = running.done;
        let process = tokio::spawn(async move {
            while let Some(frame) = output.recv().await {
                match frame {
                    ServerFrame::Stdout { chunk } => {
                        if stdout_tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                    ServerFrame::Stderr { chunk } => {
                        let mut tail = tail.lock();
                        tail.push_str(&chunk);
                        if tail.len() > STDERR_TAIL {
                            let cut = tail.ceil_char_boundary(tail.len() - STDERR_TAIL);
                            tail.drain(..cut);
                        }
                    }
                    _ => {}
                }
            }
            drop(stdout_tx);
            let reason = match done.await {
                Ok(Ok(summary)) if summary.timed_out => {
                    "it reached its maximum lifetime".to_string()
                }
                Ok(Ok(summary)) => {
                    format!("it exited with code {}", summary.exit_code.unwrap_or(-1))
                }
                Ok(Err(e)) => format!("{e:#}"),
                Err(e) => e.to_string(),
            };
            let stderr = stderr.lock().trim().to_string();
            let _ = ended_tx.send(match stderr.is_empty() {
                true => reason,
                false => format!("{reason}; it said: {stderr}"),
            });
        });
        Ok(Launched {
            stdin: running.stdin,
            stdout,
            ended,
            process: Some(AbortOnDropHandle::new(process)),
        })
    }
}

/// Why a question to a language server went unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspError {
    /// The server could not be started.
    Unavailable(String),
    /// It is still loading the project; it keeps loading for the next call.
    NotReady(String),
    /// All the servers this hub may run are busy.
    Busy(usize),
    /// It answered, but not with an edit or a result.
    Refused(String),
    /// It went away or stopped answering, and must not be asked again.
    Broken(String),
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why)
            | Self::NotReady(why)
            | Self::Refused(why)
            | Self::Broken(why) => f.write_str(why),
            Self::Busy(max) => write!(
                f,
                "all {max} language servers this hub may run are busy; try again shortly, or use replace_definition"
            ),
        }
    }
}

/// How long a server may take.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub startup: Duration,
    pub request: Duration,
}

/// A document as it is on disk.
#[derive(Debug, Clone)]
pub struct Document {
    pub path: PathBuf,
    pub text: String,
}

pub fn uri_of(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(String::from)
        .unwrap_or_else(|()| format!("file://{}", path.display()))
}

pub fn path_of(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

/// How long diagnostics may keep changing before the last are taken.
const DIAGNOSTICS_SETTLE: Duration = Duration::from_millis(750);

pub struct LanguageServer {
    pub spec: ServerSpec,
    rpc: Rpc,
    encoding: Encoding,
    ready: bool,
    /// Documents open on the server, by uri, with their versions. Each
    /// question closes what it opened; a question dropped midway leaves
    /// them for the next one to close.
    open: HashMap<String, i64>,
    next_version: i64,
    ended: Mutex<Option<oneshot::Receiver<String>>>,
    _process: Option<AbortOnDropHandle<()>>,
}

impl LanguageServer {
    /// Start the server for `spec` and initialise it.
    pub async fn start(
        spec: ServerSpec,
        launcher: &dyn Launcher,
        timeouts: Timeouts,
        max_memory_mb: u64,
    ) -> Result<Self, LspError> {
        let launched = launcher
            .launch(&spec)
            .await
            .map_err(LspError::Unavailable)?;
        let root_uri = uri_of(&spec.root);
        let folder_name = spec
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let folders = json!([{"uri": root_uri, "name": folder_name}]);
        let rpc = Rpc::new(launched.stdin, launched.stdout, folders.clone());
        let mut server = Self {
            rpc,
            encoding: Encoding::Utf16,
            ready: false,
            open: HashMap::new(),
            next_version: 0,
            ended: Mutex::new(Some(launched.ended)),
            _process: launched.process,
            spec,
        };
        let params = json!({
            "processId": null,
            "rootUri": root_uri,
            "workspaceFolders": folders,
            "capabilities": {
                "general": {"positionEncodings": ["utf-8", "utf-16"]},
                "workspace": {
                    "applyEdit": false,
                    "configuration": false,
                    "workspaceFolders": true,
                    "workspaceEdit": {"documentChanges": true, "resourceOperations": []},
                },
                "textDocument": {
                    "synchronization": {"didSave": false},
                    "publishDiagnostics": {"versionSupport": true},
                    "rename": {"prepareSupport": false},
                    "definition": {"linkSupport": true},
                    "references": {},
                },
                "window": {"workDoneProgress": true},
                "experimental": {"serverStatusNotification": true},
            },
            "initializationOptions": server.spec.kind.initialization_options(max_memory_mb),
        });
        let initialized = server
            .rpc
            .request("initialize", params, timeouts.startup)
            .await;
        let capabilities = match initialized {
            Ok(result) => result.get("capabilities").cloned().unwrap_or_default(),
            Err(RpcError::Server(why)) => {
                return Err(LspError::Unavailable(format!(
                    "{} refused to start: {why}",
                    server.spec.kind.name()
                )))
            }
            Err(e) => return Err(LspError::Unavailable(server.failure(e).await)),
        };
        server.encoding = Encoding::chosen(&capabilities);
        server
            .rpc
            .notify("initialized", json!({}))
            .await
            .map_err(|e| LspError::Unavailable(e.to_string()))?;
        Ok(server)
    }

    /// Why the server failed, as well as it can be told.
    async fn failure(&self, error: RpcError) -> String {
        let name = self.spec.kind.name();
        if error == RpcError::TimedOut {
            return format!("{name} {error}");
        }
        let ended = self.ended.lock().take();
        let reason = match ended {
            Some(ended) => tokio::time::timeout(Duration::from_secs(5), ended)
                .await
                .ok()
                .and_then(Result::ok),
            None => None,
        };
        match reason {
            Some(reason) if reason.contains("Failed to spawn") => format!(
                "{name} is not available on this hub ({reason}); replace_definition and code.edit still work"
            ),
            Some(reason) => format!("{name} stopped: {reason}"),
            None => format!("{name} stopped"),
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.rpc.is_closed()
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Ask the server to exit, then let it go.
    pub async fn shutdown(self) {
        let quick = Duration::from_secs(5);
        if self
            .rpc
            .request("shutdown", Value::Null, quick)
            .await
            .is_ok()
        {
            let _ = self.rpc.notify("exit", Value::Null).await;
            let _ = tokio::time::timeout(quick, self.rpc.closed()).await;
        }
    }

    async fn broken(&self, error: RpcError) -> LspError {
        match error {
            RpcError::Server(why) => LspError::Refused(format!("{}: {why}", self.spec.kind.name())),
            other => LspError::Broken(self.failure(other).await),
        }
    }

    /// Close what an earlier, interrupted question left open, and wait for
    /// the server to have loaded the project.
    async fn prepare(&mut self, timeouts: Timeouts) -> Result<(), LspError> {
        let stale: Vec<String> = self.open.keys().cloned().collect();
        for uri in stale {
            self.close(&uri).await?;
        }
        if self.ready || !self.spec.kind.reports_quiescence() {
            self.ready = true;
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeouts.startup;
        if self.rpc.quiescent_by(deadline).await {
            self.ready = true;
            return Ok(());
        }
        match self.rpc.is_closed() {
            true => Err(LspError::Broken(self.failure(RpcError::Closed).await)),
            false => Err(LspError::NotReady(format!(
                "{} is still loading {} after {}s; it keeps loading, so try again shortly",
                self.spec.kind.name(),
                self.spec.root.display(),
                timeouts.startup.as_secs()
            ))),
        }
    }

    async fn open(&mut self, document: &Document) -> Result<(String, i64), LspError> {
        let uri = uri_of(&document.path);
        self.next_version += 1;
        let version = self.next_version;
        let opened = self
            .rpc
            .notify(
                "textDocument/didOpen",
                json!({"textDocument": {
                    "uri": uri,
                    "languageId": language_id(&document.path),
                    "version": version,
                    "text": document.text,
                }}),
            )
            .await;
        if let Err(e) = opened {
            return Err(self.broken(e).await);
        }
        self.open.insert(uri.clone(), version);
        Ok((uri, version))
    }

    async fn close(&mut self, uri: &str) -> Result<(), LspError> {
        self.open.remove(uri);
        self.rpc.forget_diagnostics(uri);
        match self
            .rpc
            .notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri}}),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => Err(self.broken(e).await),
        }
    }

    /// Ask `method` about the document at `offset` while it is open, with
    /// the version it was opened at.
    async fn ask_at(
        &mut self,
        timeouts: Timeouts,
        document: &Document,
        offset: usize,
        method: &str,
        extra: Value,
    ) -> Result<(Value, i64), LspError> {
        self.prepare(timeouts).await?;
        let (uri, version) = self.open(document).await?;
        let mut params = json!({
            "textDocument": {"uri": uri},
            "position": position(&document.text, offset, self.encoding),
        });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        let answer = self.rpc.request(method, params, timeouts.request).await;
        let closed = self.close(&uri).await;
        match answer {
            Ok(value) => closed.map(|()| (value, version)),
            Err(e) => Err(self.broken(e).await),
        }
    }

    /// The `WorkspaceEdit` renaming what is at `offset` to `new_name`, and
    /// the version the document was opened at.
    pub async fn rename(
        &mut self,
        timeouts: Timeouts,
        document: &Document,
        offset: usize,
        new_name: &str,
    ) -> Result<(Value, i64), LspError> {
        let (edit, version) = self
            .ask_at(
                timeouts,
                document,
                offset,
                "textDocument/rename",
                json!({"newName": new_name}),
            )
            .await?;
        match edit.is_null() {
            true => Err(LspError::Refused(format!(
                "{} found nothing to rename there",
                self.spec.kind.name()
            ))),
            false => Ok((edit, version)),
        }
    }

    pub async fn references(
        &mut self,
        timeouts: Timeouts,
        document: &Document,
        offset: usize,
    ) -> Result<Vec<Value>, LspError> {
        let (found, _) = self
            .ask_at(
                timeouts,
                document,
                offset,
                "textDocument/references",
                json!({"context": {"includeDeclaration": true}}),
            )
            .await?;
        Ok(locations(found))
    }

    pub async fn definition(
        &mut self,
        timeouts: Timeouts,
        document: &Document,
        offset: usize,
    ) -> Result<Vec<Value>, LspError> {
        let (found, _) = self
            .ask_at(
                timeouts,
                document,
                offset,
                "textDocument/definition",
                json!({}),
            )
            .await?;
        Ok(locations(found))
    }

    /// The diagnostics the server publishes for the document once opened;
    /// `None` when it publishes none in time.
    pub async fn diagnostics(
        &mut self,
        timeouts: Timeouts,
        document: &Document,
    ) -> Result<Option<Published>, LspError> {
        self.prepare(timeouts).await?;
        let after = self.rpc.published_seq();
        let (uri, _) = self.open(document).await?;
        let published = self
            .rpc
            .diagnostics_after(&uri, after, timeouts.request, DIAGNOSTICS_SETTLE)
            .await;
        self.close(&uri).await?;
        Ok(published)
    }

    /// Tell the server what `documents` hold now: opened with their text
    /// and closed again, which brings its view of each back to the disk.
    pub async fn refresh(&mut self, documents: &[Document]) -> Result<(), LspError> {
        for document in documents {
            let (uri, _) = self.open(document).await?;
            self.close(&uri).await?;
        }
        Ok(())
    }
}

/// Locations from a `Location`, `Location[]` or `LocationLink[]` answer.
fn locations(found: Value) -> Vec<Value> {
    let found = match found {
        Value::Array(found) => found,
        Value::Null => Vec::new(),
        one => vec![one],
    };
    found
        .into_iter()
        .filter_map(|location| match location.get("targetUri") {
            Some(uri) => Some(json!({
                "uri": uri,
                "range": location.get("targetSelectionRange").or(location.get("targetRange"))?,
            })),
            None => location.get("uri").is_some().then_some(location),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_is_not_started_on_a_host_without_room_for_its_limit() {
        let host = MemoryReport {
            meminfo: "MemTotal:       16000000 kB\nMemFree:          100000 kB\nMemAvailable:    4194304 kB\n".into(),
            limit: "max\n".into(),
            ..MemoryReport::default()
        };
        assert_eq!(host.available_mb(), Some(4096));
        let contained = MemoryReport {
            limit: (3u64 << 30).to_string(),
            used: format!("{}\n", 2u64 << 30),
            stat: format!("anon 5\nfile {}\nfile_mapped 7\n", 1u64 << 29),
            ..host
        };
        assert_eq!(
            contained.available_mb(),
            Some(1536),
            "a container limit below the host's free memory is the room there is"
        );
        assert_eq!(MemoryReport::default().available_mb(), None);

        assert_eq!(admit(Some(4096), 2048, "rust-analyzer"), Ok(()));
        let refused = admit(Some(4096), 3584, "rust-analyzer").unwrap_err();
        assert!(refused.contains("4096 MiB available"), "{refused}");
        assert_eq!(admit(None, 1 << 20, "rust-analyzer"), Ok(()));
    }

    #[test]
    fn a_server_starts_in_its_project_with_no_secrets_and_no_toolchain_downloads() {
        let launcher = ProcessLauncher {
            runner: None,
            lifetime: Duration::from_secs(60),
            max_rss_bytes: 1 << 30,
        };
        let spec = |kind, command: &[&str]| ServerSpec {
            kind,
            root: PathBuf::from("/work/app"),
            command: command.iter().map(|part| part.to_string()).collect(),
        };
        let rust = launcher
            .request(&spec(ServerKind::RustAnalyzer, &["rust-analyzer"]))
            .unwrap();
        assert_eq!(rust.exec.workdir.as_deref(), Some("/work/app"));
        assert_eq!(
            rust.exec.env,
            [("RUSTUP_AUTO_INSTALL".to_string(), "0".to_string())]
        );
        assert_eq!(rust.max_rss_bytes, Some(1 << 30));
        assert_eq!((rust.exec.timeout_secs, rust.exec.allow_retry), (60, false));

        let typescript = launcher
            .request(&spec(
                ServerKind::TypeScript,
                &["typescript-language-server", "--stdio"],
            ))
            .unwrap();
        assert_eq!(typescript.exec.args, ["--stdio"]);
        assert!(typescript.exec.env.is_empty());
        assert!(launcher
            .request(&spec(ServerKind::TypeScript, &[]))
            .is_err());

        assert_eq!(
            ServerKind::TypeScript.initialization_options(2048)["maxTsServerMemory"],
            1536
        );
    }
}
