//! The single command-execution core, shared by the in-process fallback and the
//! runner daemon. Spawns the program from a controlled environment, confines the
//! workdir, streams output chunks to a sink, enforces the timeout and the 1 MiB
//! combined-output cap, retries transient-stderr failures, and classifies the
//! exit.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use eyre::Result;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;
use tracing::warn;

use super::env_policy::EnvPolicy;
use super::process_group::GroupChild;
use super::protocol::{ExecRequest, ExecSummary, InteractiveRequest, ServerFrame};

/// How the child's environment is built.
///
/// The two modes correspond to the executor's two paths:
///
/// - [`EnvMode::Inherit`] is the **in-process** path (`runner_socket = None`),
///   which runs commands as the same Unix user as the hub. It reproduces
///   today's behaviour exactly: the child inherits the parent environment, minus
///   only the named variables: the three secret vars, for `exec.run`, the
///   `code.*` runners and the coding sandbox alike. No env-scrub happens here — scrubbing is a
///   property of the security boundary, which the in-process path does not cross.
/// - [`EnvMode::Scrubbed`] is the **runner daemon** path (`runner_socket =
///   Some`), the audit-F7 boundary where commands run as the unprivileged
///   `agent` user. It discards the parent environment and rebuilds a clean one
///   from [`EnvPolicy`]'s allowlist plus the injected toolchain dirs, so a
///   command can never read the hub's secrets out of its own environment.
#[derive(Debug, Clone, Copy)]
pub enum EnvMode {
    /// Inherit the parent env, removing only `remove` (the in-process
    /// behaviour).
    Inherit { remove: &'static [&'static str] },
    /// Discard the parent env and rebuild it from the allowlist + toolchain
    /// injection (the daemon's security boundary).
    Scrubbed,
}

/// Apply `mode` to `cmd`'s environment. `Inherit` leaves the inherited parent
/// env in place and only removes the named vars (no `env_clear`, so the child
/// sees the same environment it did before the executor existed); `Scrubbed`
/// clears it and copies forward the allowlisted, toolchain-injected env.
fn apply_env(cmd: &mut Command, mode: &EnvMode, overrides: &[(String, String)]) {
    match mode {
        EnvMode::Inherit { remove } => {
            for var in *remove {
                cmd.env_remove(var);
            }
        }
        EnvMode::Scrubbed => {
            // Containment (audit F7): build the child env from scratch off the
            // allowlist — NEVER inherit the parent's full/secret environment,
            // which would re-expose the vault key, gateway token, and JWT secret.
            cmd.env_clear();
            for (key, value) in EnvPolicy::build(&EnvPolicy::current_env()) {
                cmd.env(key, value);
            }
        }
    }
    // After the allowlist, so a hub-composed entry is additive and wins. The
    // guest cannot reach this — the hub builds the map from its own vault.
    for (key, value) in overrides {
        cmd.env(key, value);
    }
}

/// Maximum combined stdout+stderr size, applied as the stream is consumed.
pub const MAX_OUTPUT_SIZE: usize = 1024 * 1024; // 1 MiB

/// Hard cap on retries to prevent infinite loops.
pub const MAX_RETRIES: u32 = 5;

/// Size of each streamed output chunk read from the child's pipes.
const READ_CHUNK_SIZE: usize = 64 * 1024;

/// Patterns that indicate transient/retryable failures.
const TRANSIENT_ERROR_PATTERNS: &[&str] = &[
    "connection refused",
    "connection timed out",
    "connection reset",
    "could not resolve host",
    "network is unreachable",
    "temporary failure in name resolution",
    "resource temporarily unavailable",
    "lock file",
    "unable to lock",
    "index.lock",
    "device or resource busy",
];

/// Check if stderr output indicates a transient (retryable) error.
pub fn is_transient_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    TRANSIENT_ERROR_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Validate that `workdir` resolves within one of the `allowed` directories.
/// An empty allowlist permits any path (used by the daemon, which relies on the
/// hub's pre-dispatch check plus OS-level agent-user confinement).
pub fn validate_workdir(workdir: &str, allowed: &[PathBuf]) -> Result<PathBuf, String> {
    let path = PathBuf::from(workdir);

    if allowed.is_empty() {
        return Ok(path);
    }

    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

    for allowed_dir in allowed {
        let allowed_canonical = allowed_dir
            .canonicalize()
            .unwrap_or_else(|_| allowed_dir.clone());
        if canonical.starts_with(&allowed_canonical) {
            return Ok(path);
        }
    }

    Err(format!(
        "Working directory '{workdir}' is outside allowed paths"
    ))
}

/// Resolve a finished child's exit to an `Option<i32>`, translating a signal
/// kill into the shell's `128 + signal` convention (mirrors `claude_code.rs`),
/// so an OOM-killed child reads as `137` instead of a meaningless `None`/`-1`.
fn exit_code_of(status: &std::process::ExitStatus) -> Option<i32> {
    if let Some(code) = status.code() {
        return Some(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Some(128 + sig);
        }
    }
    None
}

/// Run a command to completion, streaming output to `sink` and returning the
/// summary. `allowed_workdirs` is empty on the daemon (which trusts the hub's
/// pre-dispatch validation) and populated for the in-process path. `env_mode`
/// selects how the child's environment is built: [`EnvMode::Inherit`] for the
/// in-process path (today's pass-through behaviour) and [`EnvMode::Scrubbed`]
/// for the daemon (the audit-F7 boundary).
pub async fn run_exec(
    req: &ExecRequest,
    allowed_workdirs: &[PathBuf],
    env_mode: EnvMode,
    sink: mpsc::Sender<ServerFrame>,
) -> Result<ExecSummary> {
    preflight(req, allowed_workdirs)?;

    let timeout = Duration::from_secs(req.timeout_secs);
    let max_retries = if req.allow_retry { MAX_RETRIES } else { 0 };

    let mut attempts: u32 = 0;
    let mut retries_left = max_retries;

    loop {
        attempts += 1;

        let mut cmd = command(req, &env_mode);
        cmd.stdin(match req.stdin {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = match GroupChild::spawn(&mut cmd) {
            Ok(child) => child,
            Err(e) => return Err(eyre::eyre!("Failed to spawn command: {e}")),
        };

        // Write the payload and close, so a child reading to EOF is not left
        // waiting on a pipe that never closes. Detached: a child that never
        // reads stdin must not deadlock the run.
        if let (Some(payload), Some(mut pipe)) = (req.stdin.clone(), child.stdin.take()) {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = pipe.write_all(payload.as_bytes()).await;
                let _ = pipe.shutdown().await;
            });
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Stream stdout straight to the sink; capture stderr both to the sink
        // (for the agent) and into a buffer (to test for transient retry). The
        // pumps die with this future, so a dropped run leaves no reader holding
        // the sink open.
        let stdout_sink = sink.clone();
        let mut stdout_task = AbortOnDropHandle::new(tokio::spawn(async move {
            match stdout {
                Some(r) => pump(r, stdout_sink, true, false).await,
                None => (0usize, String::new()),
            }
        }));
        let stderr_sink = sink.clone();
        let mut stderr_task = AbortOnDropHandle::new(tokio::spawn(async move {
            match stderr {
                Some(r) => pump(r, stderr_sink, false, true).await,
                None => (0usize, String::new()),
            }
        }));

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(wait) => {
                let status = wait.map_err(|e| eyre::eyre!("Failed to execute command: {e}"))?;
                let ((stdout_bytes, _), (stderr_bytes, stderr_text)) = child
                    .drain_readers(PIPE_DRAIN_DEADLINE, &mut stdout_task, &mut stderr_task)
                    .await
                    .unwrap_or_default();
                let truncated = stdout_bytes.saturating_add(stderr_bytes) > MAX_OUTPUT_SIZE;

                let exit_code = exit_code_of(&status);
                let success = status.success();

                if !success && retries_left > 0 && is_transient_error(&stderr_text) {
                    retries_left -= 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                return Ok(ExecSummary {
                    exit_code,
                    attempts,
                    truncated,
                    timed_out: false,
                });
            }
            Err(_) => {
                warn!(
                    timeout_secs = req.timeout_secs,
                    "command timed out, killing its process group"
                );
                child.kill_group();
                let _ = child.wait().await;
                return Ok(ExecSummary {
                    exit_code: None,
                    attempts,
                    truncated: false,
                    timed_out: true,
                });
            }
        }
    }
}

/// Refuse a request whose workdir or environment overrides are not allowed,
/// before anything is spawned.
fn preflight(req: &ExecRequest, allowed_workdirs: &[PathBuf]) -> Result<()> {
    if let Some(ref workdir) = req.workdir {
        validate_workdir(workdir, allowed_workdirs).map_err(|reason| eyre::eyre!(reason))?;
    }
    super::env_policy::validate_overrides(&req.env).map_err(|reason| eyre::eyre!(reason))
}

fn command(req: &ExecRequest, env_mode: &EnvMode) -> Command {
    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args);
    apply_env(&mut cmd, env_mode, &req.env);
    if let Some(ref workdir) = req.workdir {
        cmd.current_dir(workdir);
    }
    cmd
}

/// Run an interactive command until it exits, its lifetime runs out or its
/// process group outgrows its memory limit: `stdin` is written to it as it
/// arrives and closed when it ends, and its output streams to `sink` with no
/// cap, since a language server answers for as long as it runs. Never
/// retried. Going over the memory limit kills the group and fails.
pub async fn run_interactive(
    req: &InteractiveRequest,
    allowed_workdirs: &[PathBuf],
    env_mode: EnvMode,
    sink: mpsc::Sender<ServerFrame>,
    mut stdin: mpsc::Receiver<String>,
) -> Result<ExecSummary> {
    let exec = &req.exec;
    preflight(exec, allowed_workdirs)?;
    let mut cmd = command(exec, &env_mode);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        GroupChild::spawn(&mut cmd).map_err(|e| eyre::eyre!("Failed to spawn command: {e}"))?;

    let pipe = child.stdin.take();
    let first = exec.stdin.clone();
    let _feeder = AbortOnDropHandle::new(tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let Some(mut pipe) = pipe else { return };
        if let Some(first) = first {
            if pipe.write_all(first.as_bytes()).await.is_err() {
                return;
            }
        }
        while let Some(chunk) = stdin.recv().await {
            if pipe.write_all(chunk.as_bytes()).await.is_err() || pipe.flush().await.is_err() {
                return;
            }
        }
        let _ = pipe.shutdown().await;
    }));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_sink = sink.clone();
    let mut stdout_task = AbortOnDropHandle::new(tokio::spawn(async move {
        match stdout {
            Some(r) => pump(r, stdout_sink, true, false).await,
            None => (0usize, String::new()),
        }
    }));
    let mut stderr_task = AbortOnDropHandle::new(tokio::spawn(async move {
        match stderr {
            Some(r) => pump(r, sink, false, false).await,
            None => (0usize, String::new()),
        }
    }));

    let pgid = child.pgid();
    tokio::select! {
        waited = tokio::time::timeout(Duration::from_secs(exec.timeout_secs), child.wait()) => match waited {
            Ok(status) => {
                let status = status.map_err(|e| eyre::eyre!("Failed to execute command: {e}"))?;
                let _ = child
                    .drain_readers(PIPE_DRAIN_DEADLINE, &mut stdout_task, &mut stderr_task)
                    .await;
                Ok(ExecSummary {
                    exit_code: exit_code_of(&status),
                    attempts: 1,
                    truncated: false,
                    timed_out: false,
                })
            }
            Err(_) => {
                child.kill_group();
                let _ = child.wait().await;
                Ok(ExecSummary {
                    exit_code: None,
                    attempts: 1,
                    truncated: false,
                    timed_out: true,
                })
            }
        },
        used = super::memory::exceeded(pgid, req.max_rss_bytes) => {
            child.kill_group();
            let _ = child.wait().await;
            Err(eyre::eyre!(
                "the command was stopped for using {} MiB, over its {} MiB memory limit",
                used >> 20,
                req.max_rss_bytes.unwrap_or_default() >> 20
            ))
        }
    }
}

/// How long the pumps get to finish after the leader exited, before a still-open
/// pipe is taken as proof that a descendant inherited it.
const PIPE_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Incremental UTF-8 decoder that emits only complete characters, retaining a
/// partial multi-byte sequence split across read boundaries for the next chunk.
/// Without this, a 64 KiB read can split a codepoint and a naive per-read
/// `from_utf8_lossy` mangles both halves into U+FFFD; here only a truly invalid
/// sequence (or the leftover tail at EOF) is replaced.
#[derive(Default)]
struct Utf8ChunkDecoder {
    leftover: Vec<u8>,
}

impl Utf8ChunkDecoder {
    /// Decode the complete-character prefix of `leftover + bytes`, retaining any
    /// trailing incomplete sequence; invalid (not merely incomplete) bytes are
    /// replaced lossily so the decoder never wedges. Loops so all decodable
    /// content past an invalid byte is emitted in this call.
    fn push(&mut self, bytes: &[u8]) -> String {
        self.leftover.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.leftover) {
                Ok(s) => {
                    out.push_str(s);
                    self.leftover.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(&String::from_utf8_lossy(&self.leftover[..valid]));
                    match e.error_len() {
                        // Incomplete trailing sequence: keep it for the next read.
                        None => {
                            self.leftover.drain(..valid);
                            return out;
                        }
                        // Genuinely invalid: replace it and continue scanning.
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.leftover.drain(..valid + len);
                        }
                    }
                }
            }
        }
    }

    /// Flush any retained tail at EOF, decoding it lossily.
    fn finish(self) -> String {
        if self.leftover.is_empty() {
            String::new()
        } else {
            String::from_utf8_lossy(&self.leftover).into_owned()
        }
    }
}

/// Stream a child pipe to the sink as `Stdout`/`Stderr` events. Returns the
/// total bytes read and (when `capture`) the decoded text up to the output cap,
/// which the retry classifier inspects without re-reading the consumed stream.
async fn pump<R>(
    reader: R,
    sink: mpsc::Sender<ServerFrame>,
    is_stdout: bool,
    capture: bool,
) -> (usize, String)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut buf = vec![0u8; READ_CHUNK_SIZE];
    let mut decoder = Utf8ChunkDecoder::default();
    let mut total = 0usize;
    let mut captured = String::new();

    let emit = |chunk: String, captured: &mut String| {
        if chunk.is_empty() {
            return None;
        }
        if capture && captured.len() < MAX_OUTPUT_SIZE {
            captured.push_str(&chunk);
        }
        Some(if is_stdout {
            ServerFrame::Stdout { chunk }
        } else {
            ServerFrame::Stderr { chunk }
        })
    };

    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n;
                if let Some(event) = emit(decoder.push(&buf[..n]), &mut captured) {
                    if sink.send(event).await.is_err() {
                        return (total, captured);
                    }
                }
            }
        }
    }

    if let Some(event) = emit(decoder.finish(), &mut captured) {
        let _ = sink.send(event).await;
    }
    (total, captured)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env-mode tests mutate process-global vars (`HTTPS_PROXY`,
    /// `WORKFORCE_VAULT_KEY`) and libtest runs tests on parallel threads, so
    /// unsynchronized they race: CI observed a sibling's `remove_var` landing
    /// between one test's `set_var` and its child spawn (`proxy=[absent]`
    /// instead of the set value). Every test that touches these vars takes
    /// this lock first.
    static ENV_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn req(program: &str, args: &[&str]) -> ExecRequest {
        ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            workdir: None,
            timeout_secs: 30,
            allow_retry: false,
            env: Vec::new(),
            stdin: None,
            scope: None,
            mcp: None,
        }
    }

    #[tokio::test]
    async fn a_hub_supplied_variable_reaches_the_child() {
        let mut r = req("sh", &["-c", "printf %s \"$CLAUDE_CODE_OAUTH_TOKEN\""]);
        r.env = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sub-token".into())];
        let (out, _, summary) = run_collect(&r, &[]).await;
        assert_eq!(out, "sub-token");
        assert_eq!(summary.exit_code, Some(0));
    }

    /// I1: Claude Code prefers ANTHROPIC_API_KEY over the subscription token,
    /// and the hub process carries that key. The scrubbed environment must not
    /// forward it, or every dispatched turn silently bills the metered API.
    #[tokio::test]
    async fn the_anthropic_api_key_never_reaches_a_scrubbed_child() {
        let _guard = ENV_MUTATION_LOCK.lock().await;
        std::env::set_var(
            "ANTHROPIC_API_KEY",
            concat!("sk-ant", "-must-not-reach-claude"),
        );
        let mut r = req("sh", &["-c", "printf %s \"${ANTHROPIC_API_KEY:-absent}\""]);
        r.env = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sub-token".into())];
        let (out, _, _) = run_collect(&r, &[]).await;
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert_eq!(out, "absent");
    }

    /// The override is applied after the allowlist, so it wins rather than
    /// being overwritten by the copied-forward value.
    #[tokio::test]
    async fn a_hub_supplied_variable_overrides_an_allowlisted_one() {
        let mut r = req("sh", &["-c", "printf %s \"$TZ\""]);
        r.env = vec![("TZ".into(), "Antarctica/Troll".into())];
        let (out, _, _) = run_collect(&r, &[]).await;
        assert_eq!(out, "Antarctica/Troll");
    }

    #[tokio::test]
    async fn stdin_is_written_and_closed() {
        let mut r = req("cat", &[]);
        r.stdin = Some("prompt payload".into());
        let (out, _, summary) = run_collect(&r, &[]).await;
        assert_eq!(out, "prompt payload");
        assert_eq!(summary.exit_code, Some(0), "cat must see EOF and exit");
    }

    /// A child that never reads stdin must not wedge the run on an unread pipe.
    #[tokio::test]
    async fn a_child_ignoring_stdin_still_completes() {
        let mut r = req("sh", &["-c", "printf done"]);
        r.stdin = Some("x".repeat(64 * 1024));
        let (out, _, summary) = run_collect(&r, &[]).await;
        assert_eq!(out, "done");
        assert!(!summary.timed_out);
    }

    #[tokio::test]
    async fn a_malformed_environment_is_refused_before_spawning() {
        let mut r = req("sh", &["-c", "echo should-not-run"]);
        r.env = vec![("not a name".into(), "v".into())];
        let (tx, _rx) = mpsc::channel::<ServerFrame>(64);
        assert!(run_exec(&r, &[], EnvMode::Scrubbed, tx).await.is_err());
    }

    /// Drain the sink concurrently with the producer to avoid a bounded-channel
    /// deadlock, returning `(stdout, stderr, summary)`. Defaults to the daemon's
    /// `Scrubbed` mode; [`run_collect_mode`] picks an explicit mode.
    async fn run_collect(req: &ExecRequest, allowed: &[PathBuf]) -> (String, String, ExecSummary) {
        run_collect_mode(req, allowed, EnvMode::Scrubbed).await
    }

    async fn run_collect_mode(
        req: &ExecRequest,
        allowed: &[PathBuf],
        env_mode: EnvMode,
    ) -> (String, String, ExecSummary) {
        let (tx, mut rx) = mpsc::channel::<ServerFrame>(64);
        let drain = tokio::spawn(async move {
            let mut out = String::new();
            let mut err = String::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    ServerFrame::Stdout { chunk } => out.push_str(&chunk),
                    ServerFrame::Stderr { chunk } => err.push_str(&chunk),
                    _ => {}
                }
            }
            (out, err)
        });
        let summary = run_exec(req, allowed, env_mode, tx).await.unwrap();
        let (out, err) = drain.await.unwrap();
        (out, err, summary)
    }

    /// A descendant that inherits the pipe must not keep the run open after
    /// the leader has exited: the group is killed and the output already
    /// streamed is returned.
    #[tokio::test]
    async fn a_descendant_holding_the_pipe_does_not_outlive_the_leader() {
        let mut r = req("sh", &["-c", "sleep 30 & echo hi"]);
        r.timeout_secs = 20;
        let started = std::time::Instant::now();
        let (out, _, summary) = run_collect(&r, &[]).await;
        assert_eq!(out.trim(), "hi");
        assert_eq!(summary.exit_code, Some(0));
        assert!(!summary.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the pipe held by the grandchild must not stall the drain"
        );
    }

    #[tokio::test]
    async fn streams_stdout_and_exit_zero() {
        let (out, _err, summary) = run_collect(&req("echo", &["hello"]), &[]).await;
        assert_eq!(out.trim(), "hello");
        assert_eq!(summary.exit_code, Some(0));
        assert_eq!(summary.attempts, 1);
        assert!(!summary.truncated);
        assert!(!summary.timed_out);
    }

    #[tokio::test]
    async fn non_zero_exit_is_reported() {
        let (_out, _err, summary) = run_collect(&req("sh", &["-c", "exit 42"]), &[]).await;
        assert_eq!(summary.exit_code, Some(42));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let (_out, err, summary) = run_collect(&req("sh", &["-c", "echo boom >&2"]), &[]).await;
        assert_eq!(err.trim(), "boom");
        assert_eq!(summary.exit_code, Some(0));
    }

    #[tokio::test]
    async fn scrubbed_mode_drops_secrets_and_non_allowlisted() {
        let _env = ENV_MUTATION_LOCK.lock().await;
        // The daemon's `Scrubbed` env: a secret AND an ordinary non-allowlisted
        // var (a proxy var) set in the parent must both be absent in the child.
        unsafe {
            std::env::set_var("WORKFORCE_VAULT_KEY", "must-not-leak");
            std::env::set_var("HTTPS_PROXY", "http://proxy.internal:8080");
        }
        let (out, _err, summary) = run_collect_mode(
            &req(
                "sh",
                &[
                    "-c",
                    "echo key=[${WORKFORCE_VAULT_KEY:-absent}] proxy=[${HTTPS_PROXY:-absent}]",
                ],
            ),
            &[],
            EnvMode::Scrubbed,
        )
        .await;
        unsafe {
            std::env::remove_var("WORKFORCE_VAULT_KEY");
            std::env::remove_var("HTTPS_PROXY");
        }
        assert_eq!(out.trim(), "key=[absent] proxy=[absent]");
        assert_eq!(summary.exit_code, Some(0));
    }

    /// Inert default (`runner_socket = None`): the in-process `code.*` mode
    /// (`Inherit { remove: [] }`) passes a non-allowlisted parent var straight
    /// through — proving the executor changes nothing until it is enabled.
    #[tokio::test]
    async fn inherit_mode_passes_through_non_allowlisted_var() {
        let _env = ENV_MUTATION_LOCK.lock().await;
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://proxy.internal:8080");
        }
        let (out, _err, summary) = run_collect_mode(
            &req("sh", &["-c", "echo proxy=[${HTTPS_PROXY:-absent}]"]),
            &[],
            EnvMode::Inherit { remove: &[] },
        )
        .await;
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
        }
        assert_eq!(out.trim(), "proxy=[http://proxy.internal:8080]");
        assert_eq!(summary.exit_code, Some(0));
    }

    /// In-process `exec.run` mode (`Inherit { remove: secrets }`) keeps today's
    /// behaviour: ordinary vars pass through, only the three secrets are removed.
    #[tokio::test]
    async fn inherit_mode_removes_only_named_secrets() {
        let _env = ENV_MUTATION_LOCK.lock().await;
        unsafe {
            std::env::set_var("WORKFORCE_VAULT_KEY", "must-not-leak");
            std::env::set_var("HTTPS_PROXY", "http://proxy.internal:8080");
        }
        let (out, _err, summary) = run_collect_mode(
            &req(
                "sh",
                &[
                    "-c",
                    "echo key=[${WORKFORCE_VAULT_KEY:-absent}] proxy=[${HTTPS_PROXY:-absent}]",
                ],
            ),
            &[],
            EnvMode::Inherit {
                remove: &[
                    "WORKFORCE_VAULT_KEY",
                    "WORKFORCE_GATEWAY_TOKEN",
                    "WORKFORCE_JWT_SECRET",
                ],
            },
        )
        .await;
        unsafe {
            std::env::remove_var("WORKFORCE_VAULT_KEY");
            std::env::remove_var("HTTPS_PROXY");
        }
        assert_eq!(
            out.trim(),
            "key=[absent] proxy=[http://proxy.internal:8080]"
        );
        assert_eq!(summary.exit_code, Some(0));
    }

    #[tokio::test]
    async fn workdir_escape_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = vec![tmp.path().to_path_buf()];
        let mut bad = req("ls", &[]);
        bad.workdir = Some("/etc".to_string());
        let (tx, _rx) = mpsc::channel::<ServerFrame>(8);
        let err = run_exec(&bad, &allowed, EnvMode::Scrubbed, tx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside allowed"));
    }

    #[tokio::test]
    async fn timeout_kills_child() {
        let mut slow = req("sh", &["-c", "sleep 10"]);
        slow.timeout_secs = 1;
        let (_out, _err, summary) = run_collect(&slow, &[]).await;
        assert!(summary.timed_out);
        assert_eq!(summary.exit_code, None);
    }

    #[tokio::test]
    async fn combined_output_over_cap_sets_truncated() {
        // Emit > 1 MiB so the combined cap trips.
        let bytes = MAX_OUTPUT_SIZE + 4096;
        let cmd = format!("head -c {bytes} /dev/zero | tr '\\0' 'x'");
        let (_out, _err, summary) = run_collect(&req("sh", &["-c", &cmd]), &[]).await;
        assert!(summary.truncated);
    }

    #[tokio::test]
    async fn transient_failure_retries_to_cap() {
        let mut r = req("sh", &["-c", "echo 'Connection refused' >&2 && exit 1"]);
        r.allow_retry = true;
        let (_out, _err, summary) = run_collect(&r, &[]).await;
        // allow_retry caps at MAX_RETRIES, so 1 initial + MAX_RETRIES retries.
        assert_eq!(summary.attempts, 1 + MAX_RETRIES);
        assert_eq!(summary.exit_code, Some(1));
    }

    #[tokio::test]
    async fn no_retry_on_permanent_failure() {
        let mut r = req("sh", &["-c", "exit 1"]); // no transient stderr
        r.allow_retry = true;
        let (_out, _err, summary) = run_collect(&r, &[]).await;
        assert_eq!(summary.attempts, 1);
    }

    #[tokio::test]
    async fn allow_retry_false_does_not_retry() {
        let r = req("sh", &["-c", "echo 'Connection refused' >&2 && exit 1"]);
        let (_out, _err, summary) = run_collect(&r, &[]).await;
        assert_eq!(summary.attempts, 1);
    }

    #[tokio::test]
    async fn non_utf8_output_is_lossy_decoded() {
        // Emit a lone 0xFF byte (invalid UTF-8); lossy decode must not panic.
        let (out, _err, summary) = run_collect(&req("printf", &["\\377"]), &[]).await;
        assert!(out.contains('\u{FFFD}'));
        assert_eq!(summary.exit_code, Some(0));
    }

    #[test]
    fn utf8_decoder_preserves_char_split_across_reads() {
        // '€' is the 3-byte sequence E2 82 AC; feed it split across two pushes.
        let mut d = Utf8ChunkDecoder::default();
        assert_eq!(d.push(&[0xE2, 0x82]), ""); // incomplete: nothing emitted yet
        assert_eq!(d.push(&[0xAC]), "€"); // completed cleanly, not two U+FFFD
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn utf8_decoder_emits_complete_prefix_and_retains_tail() {
        let mut d = Utf8ChunkDecoder::default();
        // "a" + first two bytes of '€': emit "a", hold the partial tail.
        assert_eq!(d.push(&[b'a', 0xE2, 0x82]), "a");
        assert_eq!(d.push(&[0xAC, b'b']), "€b");
    }

    #[test]
    fn utf8_decoder_replaces_truly_invalid_bytes() {
        let mut d = Utf8ChunkDecoder::default();
        // A stray 0xFF is invalid (not incomplete) → replaced, decoding continues.
        assert_eq!(d.push(&[b'x', 0xFF, b'y']), "x\u{FFFD}y");
    }

    #[test]
    fn utf8_decoder_flushes_incomplete_tail_lossily_at_eof() {
        let mut d = Utf8ChunkDecoder::default();
        assert_eq!(d.push(&[0xE2, 0x82]), "");
        // Stream ended mid-character: the held tail is lossily flushed.
        assert_eq!(d.finish(), "\u{FFFD}");
    }
}
