//! The single dispatch-or-local entry point shared by every execution-class
//! tool (`exec.run`, the `code.*` runners, `wasm_skill.build`).
//!
//! A configured runner routes the command to the privilege-separated daemon;
//! `None` runs it in-process (dev/local). The fail-closed invariant lives
//! entirely here: when a runner is configured, a dispatch error is returned as
//! `Err` and the command is NEVER re-run in-process — falling back would
//! execute the agent's command back in the hub user, re-exposing the vault the
//! executor exists to protect (audit F7).

use std::path::PathBuf;

use eyre::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tokio_util::task::AbortOnDropHandle;

use super::client::{dispatch_exec, dispatch_interactive};
use super::protocol::{ExecRequest, ExecSummary, InteractiveRequest, ServerFrame};
use super::run_exec::{run_exec, run_interactive, EnvMode, MAX_OUTPUT_SIZE};
use super::transport::RunnerEndpoint;

/// The assembled result of running one command: the drained output plus the
/// execution summary. Callers parse `stdout`/`stderr` and inspect the summary
/// to build their own tool-specific result, exactly as they did with the raw
/// `std::process::Output` they replaced.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    /// Captured standard output, up to the 1 MiB combined cap.
    pub stdout: String,
    /// Captured standard error, up to the 1 MiB combined cap.
    pub stderr: String,
    /// Exit/timeout/retry summary as reported by the execution core.
    pub summary: ExecSummary,
}

impl ExecOutcome {
    /// Process exit code, or `-1` when the child was killed by a signal with no
    /// code (mirrors the `exec.run` convention so callers map cleanly off the
    /// `std::process::Output::status` they used to read).
    pub fn exit_code(&self) -> i32 {
        self.summary.exit_code.unwrap_or(-1)
    }

    /// Whether the command exited successfully (exit code 0 and not timed out).
    pub fn success(&self) -> bool {
        !self.summary.timed_out && self.summary.exit_code == Some(0)
    }
}

/// Run `req` through the privilege-separated daemon at `runner` (if set,
/// fail-closed on error) or in-process against `allowed_workdirs` (if `None`),
/// draining the output stream into one [`ExecOutcome`].
///
/// This is the ONE dispatch-or-local + fail-closed path; `exec.run` and every
/// `code.*` runner call it so the behaviour cannot drift between them.
///
/// `local_env` selects the child's environment on the **in-process** path only
/// (today's pass-through: [`EnvMode::Inherit`] with the per-caller removal set).
/// The dispatch path ignores it — the daemon is the security boundary and always
/// applies the scrubbed allowlist (audit F7).
pub async fn run_or_dispatch(
    runner: Option<&RunnerEndpoint>,
    req: ExecRequest,
    allowed_workdirs: &[PathBuf],
    local_env: EnvMode,
) -> Result<ExecOutcome> {
    let (stdout, stderr, summary) = match runner {
        Some(endpoint) => {
            consume(|sink| dispatch_exec(endpoint, req, sink, CancellationToken::new(), None))
                .await?
        }
        None => consume(|sink| run_exec(&req, allowed_workdirs, local_env, sink)).await?,
    };
    Ok(ExecOutcome {
        stdout,
        stderr,
        summary,
    })
}

/// A running interactive command: write to `stdin`, read its output frames
/// from `output`, and await `done` for how it ended. Dropping it ends the
/// command.
pub struct Interactive {
    pub stdin: mpsc::Sender<String>,
    pub output: mpsc::Receiver<ServerFrame>,
    pub done: AbortOnDropHandle<Result<ExecSummary>>,
}

/// Start `req` through the daemon at `runner`, fail-closed, or in-process
/// when there is none, with `local_env` as [`run_or_dispatch`] takes it.
pub fn spawn_interactive(
    runner: Option<&RunnerEndpoint>,
    req: InteractiveRequest,
    local_env: EnvMode,
) -> Interactive {
    let (stdin_tx, stdin_rx) = mpsc::channel::<String>(64);
    let (sink, output) = mpsc::channel::<ServerFrame>(64);
    let runner = runner.cloned();
    let done = tokio::spawn(async move {
        match runner {
            Some(endpoint) => {
                dispatch_interactive(&endpoint, req, sink, stdin_rx, CancellationToken::new()).await
            }
            None => run_interactive(&req, &[], local_env, sink, stdin_rx).await,
        }
    });
    Interactive {
        stdin: stdin_tx,
        output,
        done: AbortOnDropHandle::new(done),
    }
}

/// Drive a producer (local `run_exec` or remote `dispatch`) to completion,
/// draining its output stream into stdout/stderr accumulators concurrently.
/// Centralising the drain keeps the local and remote paths byte-identical.
/// `make_producer` receives the sink and returns the producer future; the sink
/// it captures is the only sender, so the stream closes when the future ends.
async fn consume<P, F>(make_producer: P) -> Result<(String, String, ExecSummary)>
where
    P: FnOnce(mpsc::Sender<ServerFrame>) -> F,
    F: std::future::Future<Output = Result<ExecSummary>>,
{
    let (tx, mut rx) = mpsc::channel::<ServerFrame>(64);
    let drain = tokio::spawn(async move {
        let mut stdout = String::new();
        let mut stderr = String::new();
        while let Some(frame) = rx.recv().await {
            match frame {
                ServerFrame::Stdout { chunk } => {
                    if stdout.len() < MAX_OUTPUT_SIZE {
                        stdout.push_str(&chunk);
                    }
                }
                ServerFrame::Stderr { chunk } => {
                    if stderr.len() < MAX_OUTPUT_SIZE {
                        stderr.push_str(&chunk);
                    }
                }
                _ => {}
            }
        }
        (stdout, stderr)
    });

    let summary = make_producer(tx).await?;
    let (stdout, stderr) = drain.await.unwrap_or_default();
    Ok((stdout, stderr, summary))
}
