//! Wire protocol shared by the dispatch client and the runner daemon.
//!
//! The serde types *are* the function signature of the cross-process call, so a
//! single module owning both the message shapes and the framing helpers keeps
//! the wire format from drifting between the two sides.

use chrono::{DateTime, Utc};
use eyre::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::setup::SetupReport;

/// Wire-protocol revision. Bumped when the daemon gains a capability a client
/// may depend on. A request that needs one asserts the daemon announced at
/// least this version, so a skewed pair fails loudly instead of running the
/// command with the new fields silently dropped.
pub const PROTOCOL_VERSION: u32 = 6;

/// Revision that introduced the environment and stdin fields.
const PROTOCOL_ENV_STDIN: u32 = 1;

/// Revision that introduced scopes, MCP bindings, cancel and heartbeat.
pub(super) const PROTOCOL_SCOPES: u32 = 2;

/// The first revision every daemon of which mounts a keyed home. Keyed homes
/// shipped between 2 and 3 without a bump of their own, and a daemon without
/// them drops the field and mounts a throwaway home instead.
const PROTOCOL_KEYED_HOMES: u32 = 3;

/// Revision whose daemon runs a job independently of any connection: started
/// once under a client-chosen id, attached to and detached from at will, and
/// read back from an offset.
pub const PROTOCOL_DURABLE_JOBS: u32 = 4;

/// Revision whose daemon writes the Codex MCP block a binding asks for with
/// every call approved, as the hub gates them itself. Revision 3 wrote it
/// with `"auto"`, under which a read-only sandbox refuses every tool not
/// marked read-only.
const PROTOCOL_CODEX_APPROVALS: u32 = 5;

/// Revision whose daemon runs an interactive command: one whose stdin the
/// client streams for as long as it runs.
const PROTOCOL_INTERACTIVE: u32 = 6;

/// Where a scope mounts its repositories and its home: the only roots a job's
/// working directory and file paths may reach.
pub const SCOPE_WORK_ROOT: &str = "/work";
pub const SCOPE_HOME: &str = "/home/agent";
/// Where a scope with no repos over a keyed home works: inside the home, so
/// what a bench holds outlives the scope the way its transcript does.
pub const SCOPE_BENCH: &str = "/home/agent/bench";

/// Where a scope's daemon writes the MCP config for a job that binds MCP, and
/// therefore the path a client passes as `--mcp-config` for it.
pub const SCOPE_MCP_CONFIG_PATH: &str = "/home/agent/.workforce/mcp_config.json";

/// Identifies one materialised workspace on a worker: the unit of isolation
/// and lifetime for everything a job touches.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScopeId(String);

impl ScopeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScopeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names one durable job on a worker. Chosen by the client, so a job started
/// by one hub process can be found again by another.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(String);

impl JobId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// A fresh id, unique for all practical purposes.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How much of each of a job's streams a client already holds, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offsets {
    pub stdout: u64,
    pub stderr: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Start a durable job, or attach to it if it already exists, and stream its
/// output from `from` until it ends.
///
/// With `exec` the job is started if the worker has none by that id, so the
/// request can be repeated after a lost connection without running the
/// command twice. Without it the request only attaches, and a job the worker
/// does not have is refused: a job lost with its worker is never started
/// again unasked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub job: JobId,
    pub scope: ScopeId,
    pub exec: Option<ExecRequest>,
    #[serde(default)]
    pub from: Offsets,
    /// How long this client will wait for the job to end.
    pub timeout_secs: u64,
}

/// Asks the daemon to give the job an MCP endpoint whose tool calls travel
/// back to the client over this connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpBinding {
    pub server_name: String,
    pub instructions: Option<String>,
    /// Asks the daemon to also write the endpoint into a file inside the
    /// scope's home as a Codex `[mcp_servers.<name>]` block, appended to
    /// whatever the hub already wrote there; `None` writes only the
    /// Claude-format JSON.
    #[serde(default)]
    pub codex_config: Option<CodexMcpConfig>,
}

/// Where and how the daemon writes Codex's view of the MCP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexMcpConfig {
    /// The `config.toml` inside the scope's home the block is appended to.
    pub path: String,
    /// How long Codex waits for one tool call.
    pub tool_timeout_secs: u64,
}

/// One `[mcp_servers.<name>]` entry as Codex reads it: an HTTP endpoint with a
/// bearer header, and how long a tool call may take. Rendered through the TOML
/// serialiser so a token with quotes never breaks the file. The hub gates
/// every call itself, so Codex approves them all: under its `"auto"` mode a
/// read-only sandbox refuses any tool not marked read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexMcpServer {
    pub url: String,
    pub http_headers: std::collections::BTreeMap<String, String>,
    pub tool_timeout_sec: u64,
    pub default_tools_approval_mode: String,
}

impl CodexMcpServer {
    pub fn http(url: &str, bearer: &str, tool_timeout_secs: u64) -> Self {
        Self {
            url: url.to_string(),
            http_headers: [("Authorization".to_string(), format!("Bearer {bearer}"))]
                .into_iter()
                .collect(),
            tool_timeout_sec: tool_timeout_secs,
            default_tools_approval_mode: "approve".to_string(),
        }
    }

    /// The block as a TOML document fragment: `[mcp_servers.<name>]` and its
    /// keys, ready to append to a `config.toml`.
    pub fn toml_block(&self, server_name: &str) -> String {
        let mut servers = toml::Table::new();
        servers.insert(
            server_name.to_string(),
            toml::Value::try_from(self).expect("a table of strings and integers serialises"),
        );
        let mut root = toml::Table::new();
        root.insert("mcp_servers".to_string(), toml::Value::Table(servers));
        toml::to_string(&root).expect("a table of tables serialises")
    }
}

/// A request to execute a command, sent by the hub to the runner daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Program to spawn (e.g. `sh`, `cargo`, `git`).
    pub program: String,
    /// Arguments passed to `program`.
    pub args: Vec<String>,
    /// Working directory for the command; `None` runs in the daemon's cwd.
    pub workdir: Option<String>,
    /// Hard timeout in seconds; the daemon kills the child when it elapses.
    pub timeout_secs: u64,
    /// Whether transient-stderr failures may be retried (the daemon owns the
    /// retry loop so the client never double-executes).
    pub allow_retry: bool,
    /// Environment entries the hub adds to the child, applied *after* the
    /// daemon's allowlist so they are additive and explicit. Composed by the
    /// hub from its own vault and config — a guest can never ask for one.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Payload written to the child's stdin, which is then closed. `None`
    /// attaches `/dev/null`, as before.
    #[serde(default)]
    pub stdin: Option<String>,
    /// The materialised scope to run inside; `None` runs on the daemon's own
    /// filesystem, which is the only form the hub-local daemon accepts.
    #[serde(default)]
    pub scope: Option<ScopeId>,
    /// An MCP endpoint for the job, tunnelled back over this connection.
    #[serde(default)]
    pub mcp: Option<McpBinding>,
}

impl ExecRequest {
    /// The lowest protocol revision that carries every field this request
    /// uses. An older daemon would deserialise the newer fields away and run
    /// the command without them, which fails in ways that look ordinary — so
    /// the client refuses instead.
    pub fn required_protocol(&self) -> u32 {
        if self
            .mcp
            .as_ref()
            .is_some_and(|binding| binding.codex_config.is_some())
        {
            PROTOCOL_CODEX_APPROVALS
        } else if self.scope.is_some() || self.mcp.is_some() {
            PROTOCOL_SCOPES
        } else if !self.env.is_empty() || self.stdin.is_some() {
            PROTOCOL_ENV_STDIN
        } else {
            0
        }
    }
}

/// A long-lived command whose stdin the client streams in
/// [`ClientFrame::Stdin`] frames while its output streams back: a language
/// server. `exec.stdin`, when set, is written first; `exec.timeout_secs` is
/// its whole lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractiveRequest {
    pub exec: ExecRequest,
    /// The resident memory its process group may use; over it, the group is
    /// killed and the request fails. Enforced where `/proc` exists.
    #[serde(default)]
    pub max_rss_bytes: Option<u64>,
}

/// One repository to materialise into a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSpec {
    pub alias: String,
    /// `owner/name`, matched against the worker's golden datasets.
    pub slug: String,
    /// The branch to check out; empty means the remote's default.
    pub default_branch: String,
    pub setup: Option<String>,
    /// Per-job token for fetching; never stored by the worker.
    pub token: Option<String>,
}

/// Asks a worker to materialise a scope, or return it when it already exists.
/// What a scope mounts at its home: a throwaway clone of the home golden, or a
/// dataset kept under `key` across scopes for `ttl_secs` after its last use, so
/// a transcript and a bench outlive the scope that made them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HomeSpec {
    #[default]
    Ephemeral,
    Keyed {
        key: String,
        ttl_secs: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub scope: ScopeId,
    pub repos: Vec<RepoSpec>,
    pub ttl_secs: u64,
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub home: HomeSpec,
}

/// What a worker reports once a scope is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    /// Directory the job runs in, as seen from inside the scope.
    pub workdir: String,
    /// Each repo alias and its path inside the scope.
    pub repos: Vec<(String, String)>,
    pub setup: Vec<SetupReport>,
    /// Whether the scope already existed and was handed back untouched.
    pub reused: bool,
    /// Whether a keyed home was found from an earlier scope rather than
    /// created now.
    #[serde(default)]
    pub home_reused: bool,
    /// Whether the scope can run durable jobs. A scope made before its worker
    /// could does not have the directory they write to.
    #[serde(default)]
    pub jobs: bool,
}

/// Why the daemon declined a request before doing any work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    AtCapacity,
    PoolFull,
    UnknownScope,
    ProtocolMismatch,
    UnknownJob,
    JobsUnsupported,
    ScopeBusy,
}

impl std::fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AtCapacity => "the worker is at capacity",
            Self::PoolFull => "the worker's pool is below its free-space floor",
            Self::UnknownScope => "the scope does not exist on the worker",
            Self::ProtocolMismatch => "this daemon does not serve that kind of request",
            Self::UnknownJob => "the worker has no such job",
            Self::JobsUnsupported => "the scope predates durable jobs",
            Self::ScopeBusy => "another job is still running in the scope",
        })
    }
}

/// A worker's answer to [`Request::Health`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHealth {
    pub scopes: u32,
    #[serde(default)]
    pub homes: u32,
    pub capacity: u32,
    pub pool_free_bytes: u64,
    pub goldens: Vec<(String, DateTime<Utc>)>,
    pub version: String,
    /// Durable jobs still running.
    #[serde(default)]
    pub jobs: u32,
}

impl WorkerHealth {
    /// The health of a daemon that has no scopes to offer: the hub-local
    /// runner, which shares the hub's filesystem instead.
    pub fn without_scopes() -> Self {
        Self {
            scopes: 0,
            homes: 0,
            capacity: 0,
            pool_free_bytes: 0,
            goldens: Vec::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            jobs: 0,
        }
    }
}

/// What a client asks of the daemon. One request per connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Exec(ExecRequest),
    Interactive(InteractiveRequest),
    Prepare(PrepareRequest),
    Release {
        scope: ScopeId,
    },
    Health,
    Run(RunRequest),
    /// The client holds a finished job's result; the worker may drop it.
    Forget {
        job: JobId,
    },
}

impl Request {
    /// The lowest protocol revision that understands this request.
    pub fn required_protocol(&self) -> u32 {
        match self {
            Self::Exec(req) => req.required_protocol(),
            Self::Interactive(req) => req.exec.required_protocol().max(PROTOCOL_INTERACTIVE),
            Self::Prepare(PrepareRequest {
                home: HomeSpec::Keyed { .. },
                ..
            }) => PROTOCOL_KEYED_HOMES,
            Self::Prepare(_) | Self::Release { .. } | Self::Health => PROTOCOL_SCOPES,
            Self::Run(run) => run
                .exec
                .as_ref()
                .map_or(0, ExecRequest::required_protocol)
                .max(PROTOCOL_DURABLE_JOBS),
            Self::Forget { .. } => PROTOCOL_DURABLE_JOBS,
        }
    }

    /// How long the client waits for the daemon to finish, before its grace.
    pub fn timeout_secs(&self) -> u64 {
        match self {
            Self::Exec(req) => req.timeout_secs,
            Self::Interactive(req) => req.exec.timeout_secs,
            Self::Prepare(_) => PREPARE_TIMEOUT_SECS,
            Self::Run(run) => run.timeout_secs,
            Self::Release { .. } | Self::Health | Self::Forget { .. } => CONTROL_TIMEOUT_SECS,
        }
    }
}

const PREPARE_TIMEOUT_SECS: u64 = 600;
const CONTROL_TIMEOUT_SECS: u64 = 60;

/// Frames the client sends. The first must be [`ClientFrame::Request`]; the
/// others may follow at any time while the job runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ClientFrame {
    Request(Box<Request>),
    Cancel,
    McpResponse {
        id: u64,
        body: String,
    },
    /// Input for an interactive command's stdin.
    Stdin {
        chunk: String,
    },
    /// The interactive command's stdin ends.
    CloseStdin,
}

/// Frames the daemon sends while a job runs and to end it.
///
/// Output is streamed (never buffered whole) so a minutes-long, silent
/// `cargo build` keeps the connection live and the daemon's memory bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Sent once, before any output, announcing the daemon's protocol revision.
    Ready {
        protocol_version: u32,
    },
    Stdout {
        chunk: String,
    },
    Stderr {
        chunk: String,
    },
    /// A durable job's output: bytes `from..to` of one stream, so a client
    /// that attaches again asks for exactly what it lacks. The range is in
    /// the stream's bytes, which a chunk carrying replaced invalid UTF-8 no
    /// longer matches in length.
    Output {
        stream: Stream,
        from: u64,
        to: u64,
        chunk: String,
    },
    /// Sent on a fixed interval while a job runs so a silent peer can be told
    /// from a dead one.
    Heartbeat,
    /// A tool call from the job, to be answered with [`ClientFrame::McpResponse`].
    McpRequest {
        id: u64,
        body: String,
    },
    Prepared(Prepared),
    Healthy(WorkerHealth),
    Done(ExecSummary),
    Refused {
        reason: RefusalReason,
    },
    /// The daemon could not carry out the request; asking again would fail
    /// the same way.
    Failed {
        message: String,
    },
}

impl ServerFrame {
    /// Whether this frame ends the job.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Prepared(_)
                | Self::Healthy(_)
                | Self::Done(_)
                | Self::Refused { .. }
                | Self::Failed { .. }
        )
    }
}

/// The summary carried by [`ServerFrame::Done`], returned by the execution
/// core and by the dispatch client so both local and remote paths assemble an
/// identical `ToolResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecSummary {
    /// Process exit code, or `None` when killed by a signal with no code.
    pub exit_code: Option<i32>,
    /// Number of attempts made (1 + retries).
    pub attempts: u32,
    /// Whether the combined output hit the 1 MiB cap and was truncated.
    pub truncated: bool,
    /// Whether the command was killed because it exceeded its timeout.
    pub timed_out: bool,
}

impl ExecSummary {
    pub fn exited(code: i32) -> Self {
        Self {
            exit_code: Some(code),
            attempts: 1,
            truncated: false,
            timed_out: false,
        }
    }

    /// The summary of a job killed on the client's request: the shell's
    /// `128 + SIGKILL` convention, so callers classify it like any other
    /// signal death.
    pub fn killed() -> Self {
        Self::exited(128 + libc::SIGKILL)
    }
}

/// Upper bound on a single framed message, guarding the reader against a
/// corrupt/hostile length prefix demanding a huge allocation. Output chunks are
/// bounded well under the 1 MiB output cap, so 8 MiB is comfortably generous.
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// Encode `value` as a frame body, refusing one over the cap.
pub fn frame_body<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_LEN {
        bail!(
            "framed message too large: {} bytes exceeds the {MAX_FRAME_LEN} byte cap",
            body.len()
        );
    }
    Ok(body)
}

/// Write a `u32`-LE length prefix followed by the JSON body of `value`.
pub async fn write_framed<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    write_frame_body(writer, &frame_body(value)?).await
}

/// Write an already encoded frame body behind its length prefix.
pub async fn write_frame_body<W>(writer: &mut W, body: &[u8]) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    writer.write_all(&(body.len() as u32).to_le_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-prefixed JSON message, or `Ok(None)` on a clean EOF before
/// the next frame begins (the peer finished cleanly).
pub async fn read_framed<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_LEN {
        bail!("framed message length {len} exceeds cap");
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::super::setup::SetupOutcome;
    use super::*;

    #[test]
    fn a_codex_mcp_block_renders_as_valid_toml() {
        let http = CodexMcpServer::http("http://host.docker.internal:4321/mcp", "to\"ken", 300)
            .toml_block("workforce");
        let parsed: toml::Table = http.parse().unwrap();
        let server = &parsed["mcp_servers"]["workforce"];
        assert_eq!(
            server["url"].as_str(),
            Some("http://host.docker.internal:4321/mcp")
        );
        assert_eq!(
            server["http_headers"]["Authorization"].as_str(),
            Some("Bearer to\"ken"),
            "a quote in the token survives the serialiser"
        );
        assert_eq!(server["tool_timeout_sec"].as_integer(), Some(300));
        assert_eq!(
            server["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert!(server.get("command").is_none());
        assert!(server.get("env_vars").is_none());
    }

    #[test]
    fn a_binding_without_a_codex_config_still_deserialises() {
        let binding: McpBinding =
            serde_json::from_str(r#"{"server_name":"workforce","instructions":null}"#).unwrap();
        assert_eq!(binding.codex_config, None);
    }

    fn exec(program: &str) -> ExecRequest {
        ExecRequest {
            program: program.into(),
            args: vec![],
            workdir: None,
            timeout_secs: 5,
            allow_retry: false,
            env: vec![],
            stdin: None,
            scope: None,
            mcp: None,
        }
    }

    #[test]
    fn exec_request_round_trips_through_json() {
        let req = ExecRequest {
            args: vec!["-c".into(), "echo hi".into()],
            workdir: Some("/tmp".into()),
            allow_retry: true,
            env: vec![("FOO".into(), "bar".into())],
            stdin: Some("payload".into()),
            scope: Some(ScopeId::new("abc123")),
            mcp: Some(McpBinding {
                server_name: "workforce".into(),
                instructions: None,
                codex_config: None,
            }),
            ..exec("sh")
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn the_verify_scripts_request_is_the_frame_the_client_sends() {
        let script = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/privsep-verify.sh"
        ))
        .unwrap();
        let sent = script
            .lines()
            .find_map(|line| line.strip_prefix("REQUEST = '")?.strip_suffix('\''))
            .expect("privsep-verify.sh defines REQUEST");
        let frame = ClientFrame::Request(Box::new(Request::Exec(ExecRequest {
            args: vec!["-u".into()],
            timeout_secs: 30,
            ..exec("id")
        })));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(sent).unwrap(),
            serde_json::to_value(&frame).unwrap()
        );
        assert_eq!(serde_json::from_str::<ClientFrame>(sent).unwrap(), frame);
    }

    #[test]
    fn a_request_without_the_newer_fields_still_deserialises() {
        let legacy = r#"{"program":"sh","args":[],"workdir":null,
            "timeout_secs":5,"allow_retry":false}"#;
        let req: ExecRequest = serde_json::from_str(legacy).unwrap();
        assert!(req.env.is_empty());
        assert!(req.stdin.is_none());
        assert!(req.scope.is_none());
        assert_eq!(req.required_protocol(), 0);
    }

    #[test]
    fn the_required_protocol_follows_the_newest_field_in_use() {
        assert_eq!(exec("sh").required_protocol(), 0);
        assert_eq!(
            ExecRequest {
                env: vec![("A".into(), "b".into())],
                ..exec("sh")
            }
            .required_protocol(),
            1
        );
        assert_eq!(
            ExecRequest {
                stdin: Some(String::new()),
                ..exec("sh")
            }
            .required_protocol(),
            1
        );
        assert_eq!(
            ExecRequest {
                scope: Some(ScopeId::new("s")),
                ..exec("sh")
            }
            .required_protocol(),
            2
        );
        assert_eq!(Request::Health.required_protocol(), 2);
        let codex = ExecRequest {
            mcp: Some(McpBinding {
                server_name: "workforce".into(),
                instructions: None,
                codex_config: Some(CodexMcpConfig {
                    path: "/home/agent/.codex-1/config.toml".into(),
                    tool_timeout_secs: 300,
                }),
            }),
            ..exec("codex")
        };
        assert_eq!(
            codex.required_protocol(),
            5,
            "a daemon that would drop the codex block, or have it ask before each call, is refused"
        );
        assert_eq!(
            Request::Run(RunRequest {
                job: JobId::new("j"),
                scope: ScopeId::new("s"),
                exec: Some(codex),
                from: Offsets::default(),
                timeout_secs: 60,
            })
            .required_protocol(),
            5,
            "a durable codex job asks the same of its daemon"
        );
        assert_eq!(
            Request::Interactive(InteractiveRequest {
                exec: exec("rust-analyzer"),
                max_rss_bytes: None,
            })
            .required_protocol(),
            6,
            "a daemon that would run it with no way to feed its stdin is refused"
        );
        let prepare = |home| {
            Request::Prepare(PrepareRequest {
                scope: ScopeId::new("s"),
                repos: Vec::new(),
                ttl_secs: 60,
                env: Vec::new(),
                home,
            })
        };
        assert_eq!(prepare(HomeSpec::Ephemeral).required_protocol(), 2);
        assert_eq!(
            prepare(HomeSpec::Keyed {
                key: "thread".into(),
                ttl_secs: 60
            })
            .required_protocol(),
            3,
            "a daemon that would mount a throwaway home instead is refused"
        );
    }

    #[test]
    fn every_server_frame_round_trips() {
        for frame in [
            ServerFrame::Ready {
                protocol_version: 2,
            },
            ServerFrame::Stdout {
                chunk: "out".into(),
            },
            ServerFrame::Stderr {
                chunk: "err".into(),
            },
            ServerFrame::Heartbeat,
            ServerFrame::McpRequest {
                id: 7,
                body: "{}".into(),
            },
            ServerFrame::Prepared(Prepared {
                workdir: "/work".into(),
                repos: vec![("rails".into(), "/work/rails".into())],
                setup: vec![SetupReport {
                    alias: "rails".into(),
                    command: "bundle install".into(),
                    outcome: SetupOutcome::Failed {
                        exit_code: 2,
                        stderr_tail: "boom".into(),
                    },
                }],
                reused: true,
                home_reused: true,
                jobs: true,
            }),
            ServerFrame::Output {
                stream: Stream::Stderr,
                from: 12,
                to: 16,
                chunk: "late".into(),
            },
            ServerFrame::Healthy(WorkerHealth::without_scopes()),
            ServerFrame::Done(ExecSummary {
                exit_code: None,
                attempts: 1,
                truncated: false,
                timed_out: true,
            }),
            ServerFrame::Refused {
                reason: RefusalReason::AtCapacity,
            },
        ] {
            let json = serde_json::to_string(&frame).unwrap();
            let back: ServerFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(frame, back, "{json}");
        }
    }

    #[test]
    fn every_client_frame_round_trips() {
        for frame in [
            ClientFrame::Request(Box::new(Request::Exec(exec("true")))),
            ClientFrame::Request(Box::new(Request::Prepare(PrepareRequest {
                scope: ScopeId::new("s"),
                repos: vec![RepoSpec {
                    alias: "rails".into(),
                    slug: "acme/rails".into(),
                    default_branch: "main".into(),
                    setup: Some("bundle install".into()),
                    token: Some("t".into()),
                }],
                ttl_secs: 60,
                env: vec![],
                home: HomeSpec::Keyed {
                    key: "thread-1".into(),
                    ttl_secs: 604_800,
                },
            }))),
            ClientFrame::Request(Box::new(Request::Release {
                scope: ScopeId::new("s"),
            })),
            ClientFrame::Request(Box::new(Request::Health)),
            ClientFrame::Cancel,
            ClientFrame::McpResponse {
                id: 3,
                body: "{}".into(),
            },
            ClientFrame::Request(Box::new(Request::Interactive(InteractiveRequest {
                exec: exec("rust-analyzer"),
                max_rss_bytes: Some(1 << 30),
            }))),
            ClientFrame::Stdin {
                chunk: "Content-Length: 2\r\n\r\n{}".into(),
            },
            ClientFrame::CloseStdin,
        ] {
            let json = serde_json::to_string(&frame).unwrap();
            let back: ClientFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(frame, back, "{json}");
        }
    }

    #[test]
    fn a_prepare_without_a_home_is_ephemeral_and_a_prepared_without_one_is_not_reused() {
        let request: PrepareRequest =
            serde_json::from_str(r#"{"scope":"s","repos":[],"ttl_secs":60,"env":[]}"#).unwrap();
        assert_eq!(request.home, HomeSpec::Ephemeral);
        let prepared: Prepared =
            serde_json::from_str(r#"{"workdir":"/work","repos":[],"setup":[],"reused":false}"#)
                .unwrap();
        assert!(!prepared.home_reused);
    }

    #[test]
    fn a_v1_done_frame_parses_as_a_v2_done() {
        let v1 =
            r#"{"kind":"done","exit_code":1,"attempts":1,"truncated":false,"timed_out":false}"#;
        let frame: ServerFrame = serde_json::from_str(v1).unwrap();
        assert_eq!(
            frame,
            ServerFrame::Done(ExecSummary {
                exit_code: Some(1),
                attempts: 1,
                truncated: false,
                timed_out: false,
            })
        );
        assert!(frame.is_terminal());
    }

    #[test]
    fn an_exec_request_frame_still_reads_as_a_v1_exec_request() {
        let frame = ClientFrame::Request(Box::new(Request::Exec(ExecRequest {
            stdin: Some("prompt".into()),
            ..exec("cat")
        })));
        let json = serde_json::to_string(&frame).unwrap();
        let legacy: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(legacy.program, "cat");
        assert_eq!(legacy.stdin.as_deref(), Some("prompt"));
    }

    #[test]
    fn an_unknown_frame_kind_is_rejected() {
        assert!(serde_json::from_str::<ServerFrame>(r#"{"kind":"surprise"}"#).is_err());
        assert!(serde_json::from_str::<ClientFrame>(r#"{"frame":"surprise"}"#).is_err());
    }

    #[test]
    fn a_body_over_the_cap_is_refused_before_it_is_written() {
        let oversized = ClientFrame::Request(Box::new(Request::Exec(ExecRequest {
            stdin: Some("x".repeat(MAX_FRAME_LEN)),
            ..exec("cat")
        })));
        let err = frame_body(&oversized).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn framing_round_trips_multiple_messages() {
        let mut buf: Vec<u8> = Vec::new();
        let req = ClientFrame::Request(Box::new(Request::Exec(exec("echo"))));
        write_framed(&mut buf, &req).await.unwrap();
        write_framed(
            &mut buf,
            &ServerFrame::Stdout {
                chunk: "chunk".into(),
            },
        )
        .await
        .unwrap();

        let mut reader = std::io::Cursor::new(buf);
        let got_req: ClientFrame = read_framed(&mut reader).await.unwrap().unwrap();
        assert_eq!(got_req, req);
        let got_event: ServerFrame = read_framed(&mut reader).await.unwrap().unwrap();
        assert_eq!(
            got_event,
            ServerFrame::Stdout {
                chunk: "chunk".into()
            }
        );
        let end: Option<ServerFrame> = read_framed(&mut reader).await.unwrap();
        assert!(end.is_none());
    }

    #[tokio::test]
    async fn a_frame_over_the_cap_is_refused_by_the_reader() {
        let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let mut reader = std::io::Cursor::new(buf);
        let err = read_framed::<_, ServerFrame>(&mut reader)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds cap"), "{err}");
    }

    #[tokio::test]
    async fn truncated_length_prefix_is_eof_none() {
        let buf: Vec<u8> = vec![0x02, 0x00];
        let mut reader = std::io::Cursor::new(buf);
        let end: Option<ClientFrame> = read_framed(&mut reader).await.unwrap();
        assert!(end.is_none());
    }
}
