use std::process::Stdio;

use eyre::{eyre, Result, WrapErr};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tracing::{debug, warn};

use super::config::ObscuraSkillConfig;

/// Drop-guard for a spawned `obscura mcp` child. `kill_on_drop(true)` is set on
/// the underlying [`tokio::process::Child`], so terminating the subprocess is
/// just a matter of releasing this guard.
pub(super) struct ChildGuard {
    #[allow(dead_code)]
    child: Child,
}

pub(super) struct Spawned {
    pub(super) guard: ChildGuard,
    pub(super) stdin: ChildStdin,
    pub(super) stdout: BufReader<ChildStdout>,
}

pub(super) fn spawn_mcp(config: &ObscuraSkillConfig) -> Result<Spawned> {
    let mut cmd = Command::new(&config.obscura_binary);
    cmd.arg("mcp");
    if config.stealth {
        cmd.arg("--stealth");
    }
    if let Some(proxy) = config.proxy.as_deref().filter(|s| !s.is_empty()) {
        cmd.arg("--proxy").arg(proxy);
    }
    if let Some(ua) = config.user_agent.as_deref().filter(|s| !s.is_empty()) {
        cmd.arg("--user-agent").arg(ua);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .wrap_err_with(|| {
            format!(
                "failed to spawn obscura binary `{}` (is it on PATH or set obscura_binary in config?)",
                config.obscura_binary
            )
        })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| eyre!("obscura child has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("obscura child has no stdout"))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward_stderr_lines(BufReader::new(stderr)));
    }

    Ok(Spawned {
        guard: ChildGuard { child },
        stdin,
        stdout: BufReader::new(stdout),
    })
}

async fn forward_stderr_lines<R>(mut reader: BufReader<R>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    debug!(target: "obscura", line = trimmed, "stderr");
                }
            }
            Err(e) => {
                warn!(target: "obscura", %e, "stderr read failed");
                break;
            }
        }
    }
}

/// Window of recent spawn timestamps. Bounded retries protect against a
/// deterministic crash turning into a fork bomb: `limit` spawns inside
/// `window_ms` cause [`record`] to return `Err` until enough time elapses.
pub(super) struct RespawnBudget {
    window_ms: u64,
    limit: usize,
    timestamps: Vec<std::time::Instant>,
}

impl RespawnBudget {
    pub(super) fn new(window_ms: u64, limit: usize) -> Self {
        Self {
            window_ms,
            limit,
            timestamps: Vec::with_capacity(limit + 1),
        }
    }

    pub(super) fn record(&mut self) -> Result<()> {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(self.window_ms);
        self.timestamps.retain(|t| now.duration_since(*t) < window);
        if self.timestamps.len() >= self.limit {
            return Err(eyre!(
                "obscura has crashed {limit} times in the last {seconds}s; refusing to respawn",
                limit = self.limit,
                seconds = self.window_ms / 1000,
            ));
        }
        self.timestamps.push(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respawn_budget_allows_up_to_limit_then_rejects() {
        let mut budget = RespawnBudget::new(60_000, 3);
        for _ in 0..3 {
            budget.record().unwrap();
        }
        assert!(budget.record().is_err());
    }

    #[test]
    fn respawn_budget_clears_after_window() {
        let mut budget = RespawnBudget::new(1, 1);
        budget.record().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        budget.record().unwrap();
    }

    #[tokio::test]
    async fn spawn_fails_with_useful_error_for_missing_binary() {
        let cfg = ObscuraSkillConfig {
            obscura_binary: "/nonexistent/obscura-binary-for-test".to_string(),
            ..ObscuraSkillConfig::default()
        };
        let err = spawn_mcp(&cfg).err().expect("expected spawn failure");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("obscura"),
            "error should mention obscura: {msg}"
        );
    }
}
