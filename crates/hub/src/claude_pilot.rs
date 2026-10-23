//! Claude Pilot engine — orchestrated multi-phase Claude Code execution.
//!
//! Instead of a single long-running `claude -p` subprocess, claude-pilot
//! breaks a step into focused phases with tight tool restrictions and
//! turn budgets. Session reuse (`--resume`) preserves context and prompt
//! cache across phases.
//!
//! Each phase is a short `claude -p` call (~3-10 turns) that does one
//! thing well: setup, code, test, push. The orchestration logic lives
//! here in Rust, not in Claude's head.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use workforce_agent::runner::ProgressEvent;

use crate::claude_code::{
    claude_program, count_output, is_cancelled, next_line_lossy, read_chunks, tool_search_blocked,
    ClaudeAuth, ClaudeCodeResult, Completion, ResolvedAuth, StderrCapture,
};
use crate::subscription::Login;
use crate::vault::Vault;

/// The engine name a workflow step selects the pilot by, and the name its
/// step records and cost ledger rows carry.
pub const ENGINE: &str = "claude-pilot";

/// A single phase in a pilot execution.
#[derive(Debug, Clone)]
pub struct PilotPhase {
    pub name: String,
    pub task: String,
    pub allowed_tools: Vec<String>,
    pub max_turns: u32,
    /// Whether this phase needs MCP tools.
    pub mcp: bool,
}

/// Configuration for a complete pilot execution.
pub struct PilotConfig {
    pub phases: Vec<PilotPhase>,
    pub workdir: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The hub tools phases that ask for them are served.
    pub mcp: Option<crate::mcp_turn::RoundTools>,
    pub timeout: Duration,
    /// The agent the pilot runs for, as its login's errors name it.
    pub agent_id: String,
}

/// The login a pilot's phases run on, ready to authenticate them, and the
/// agent they run for.
struct Paid<'a> {
    login: &'a Login,
    auth: ResolvedAuth,
    caller: &'a str,
}

/// Result of a complete pilot execution (aggregated across all phases).
pub struct PilotResult {
    pub text: String,
    pub exit_code: i32,
    pub duration: Duration,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub num_turns: u32,
    pub session_id: Option<String>,
    pub phases_completed: u32,
    pub phases_total: u32,
}

/// Run a single phase of the pilot, returning the result and session ID.
#[allow(clippy::too_many_arguments)]
async fn run_phase(
    phase: &PilotPhase,
    workdir: &PathBuf,
    model: Option<&str>,
    effort: Option<&str>,
    mcp_config_path: Option<&Path>,
    session_id: Option<&str>,
    paid: &Paid<'_>,
    gh_token: Option<&str>,
    cancel: Option<&CancellationToken>,
    progress_tx: &Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
    timeout: Duration,
) -> Result<(String, i32, u64, u64, f64, u32, Option<String>)> {
    let mut cmd = Command::new(claude_program());
    cmd.arg("-p")
        .arg("--verbose")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--exclude-dynamic-system-prompt-sections");

    // Session resume from previous phase
    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid);
    }

    cmd.arg("--max-turns").arg(phase.max_turns.to_string());

    if !phase.allowed_tools.is_empty() {
        cmd.arg("--allowedTools").arg(phase.allowed_tools.join(","));
    }

    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }

    if let Some(effort) = effort {
        cmd.arg("--effort").arg(effort);
    }

    // Only pass MCP config if this phase needs it
    if let Some(mcp_path) = mcp_config_path.filter(|_| phase.mcp) {
        cmd.arg("--mcp-config").arg(mcp_path);
        if tool_search_blocked(false) {
            cmd.arg("--disallowedTools").arg("ToolSearch");
        }
    }

    cmd.current_dir(workdir);
    paid.auth.apply(&mut cmd);

    // Performance env vars
    cmd.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
    cmd.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1");
    cmd.env("CLAUDE_ENABLE_STREAM_WATCHDOG", "1");

    if let Some(token) = gh_token {
        cmd.env("GH_TOKEN", token);
        cmd.env("GITHUB_TOKEN", token);
    }

    for var in ["HOME", "PATH", "NODE_PATH"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }

    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| eyre!("Failed to spawn claude CLI for phase '{}': {e}", phase.name))?;

    // Pipe task prompt via stdin
    if let Some(mut stdin) = child.stdin.take() {
        let task_bytes = phase.task.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&task_bytes).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stderr_task = tokio::spawn(async move {
        let mut capture = StderrCapture::default();
        if let Some(reader) = stderr_handle {
            read_chunks(reader, |chunk| capture.push(chunk)).await;
        }
        capture.into_bytes()
    });

    let phase_name = phase.name.clone();
    let progress_tx_clone = progress_tx.clone();
    let stream_task = tokio::spawn(async move {
        match stdout_handle {
            Some(stdout) => {
                read_phase_stream(BufReader::new(stdout), &phase_name, &progress_tx_clone).await
            }
            None => None,
        }
    });

    // Wait for process with timeout + cancellation
    let status = if let Some(token) = cancel {
        tokio::select! {
            result = tokio::time::timeout(timeout, child.wait()) => {
                result
                    .map_err(|_| eyre!("Phase '{}' timed out after {:?}", phase.name, timeout))?
                    .map_err(|e| eyre!("Phase '{}' process error: {e}", phase.name))?
            }
            _ = token.cancelled() => {
                return Err(eyre!("Phase '{}' cancelled", phase.name));
            }
        }
    } else {
        tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| eyre!("Phase '{}' timed out after {:?}", phase.name, timeout))?
            .map_err(|e| eyre!("Phase '{}' process error: {e}", phase.name))?
    };

    let result_json = stream_task.await.ok().flatten();
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    let exit_code = status.code().unwrap_or(-1);

    let stderr_str = String::from_utf8_lossy(&stderr_bytes);
    if !stderr_str.is_empty() {
        info!(
            phase = %phase.name,
            exit_code,
            stderr = %stderr_str.chars().take(500).collect::<String>(),
            "Claude Pilot phase stderr"
        );
    }

    let failed = exit_code != 0
        || result_json
            .as_ref()
            .and_then(|v| v.get("is_error"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    let (text, input_tokens, output_tokens, cost_usd, num_turns, session_id) = match result_json {
        Some(ref v) => (
            v.get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            v.pointer("/usage/input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            v.pointer("/usage/output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            v.get("total_cost_usd")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0),
            v.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(1) as u32,
            v.get("session_id")
                .and_then(|s| s.as_str())
                .map(String::from),
        ),
        None => (String::new(), 0, 0, 0.0, 1, None),
    };
    let (completion, notice) = match failed {
        true => (Completion::Failed, text.clone()),
        false => (Completion::Success, String::new()),
    };
    paid.login.settle(
        &ClaudeCodeResult {
            completion,
            notice,
            stderr_tail: stderr_str.into_owned(),
            ..ClaudeCodeResult::default()
        }
        .round_end(),
        paid.caller,
    );

    Ok((
        text,
        exit_code,
        input_tokens,
        output_tokens,
        cost_usd,
        num_turns,
        session_id,
    ))
}

/// Read a phase's `stream-json` to its end, logging and reporting progress as
/// it goes, and return its result event. The result comes last, so no amount
/// of output before it stops the read.
async fn read_phase_stream(
    stdout: impl AsyncBufRead + Unpin,
    phase: &str,
    progress_tx: &Option<Sender<ProgressEvent>>,
) -> Option<serde_json::Value> {
    let program = format!("claude ({ENGINE} phase {phase})");
    let mut lines = stdout.split(b'\n');
    let mut result = None;
    let mut total_bytes = 0usize;
    let mut round = 0u32;
    while let Some(line) = next_line_lossy(&mut lines).await {
        total_bytes = count_output(&program, total_bytes, line.len());
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        log_phase_event(phase, &event);
        emit_phase_progress(&event, progress_tx, &mut round).await;
        if event.get("type").and_then(|t| t.as_str()) == Some("result") {
            result = Some(event);
        }
    }
    result
}

/// Run all phases of a pilot execution sequentially, resuming sessions.
pub async fn run_pilot(
    config: &PilotConfig,
    login: &Login,
    vault: Option<&Vault>,
    global_vault: Option<&Vault>,
    cancel: Option<CancellationToken>,
    progress_tx: Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
) -> Result<PilotResult> {
    let paid = Paid {
        login,
        auth: ResolvedAuth::resolve(&ClaudeAuth::Resolved(login.clone()))?,
        caller: &config.agent_id,
    };

    // Pilot phases run against a workspace with no single owner, so take the
    // same flat-then-environment token every other Claude Code round runs with.
    let gh_token = crate::claude_code::effective_gh_token(None, vault, global_vault);

    // Held for the whole pilot: every phase resumes one session, served by
    // one listener.
    let local = match &config.mcp {
        Some(tools) => Some(tools.serve_local().await?),
        None => None,
    };
    let mcp_config_path = local.as_ref().map(crate::mcp_turn::LocalMcp::path);

    let start = Instant::now();
    let total_phases = config.phases.len() as u32;
    let mut session_id: Option<String> = None;
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    let mut total_cost = 0.0f64;
    let mut total_turns = 0u32;
    let mut phases_completed = 0u32;
    let mut last_text = String::new();
    let mut last_exit_code = 0i32;

    for (i, phase) in config.phases.iter().enumerate() {
        info!(
            phase = %phase.name,
            index = i + 1,
            total = total_phases,
            session_id = ?session_id,
            max_turns = phase.max_turns,
            tools = ?phase.allowed_tools,
            "Starting pilot phase"
        );

        let cancel_ref = cancel.as_ref();
        let per_phase_timeout = Duration::from_secs(10 * 60); // 10 min per phase

        let result = run_phase(
            phase,
            &config.workdir,
            config.model.as_deref(),
            config.effort.as_deref(),
            mcp_config_path,
            session_id.as_deref(),
            &paid,
            gh_token.as_deref(),
            cancel_ref,
            &progress_tx,
            per_phase_timeout,
        )
        .await;

        match result {
            Ok((text, exit_code, input_tokens, output_tokens, cost, turns, sid)) => {
                total_input_tokens += input_tokens;
                total_output_tokens += output_tokens;
                total_cost += cost;
                total_turns += turns;
                last_text = text;
                last_exit_code = exit_code;

                if let Some(ref s) = sid {
                    session_id = Some(s.clone());
                }

                info!(
                    phase = %phase.name,
                    exit_code,
                    turns,
                    cost_usd = format!("{cost:.4}"),
                    session_id = ?sid,
                    "Pilot phase completed"
                );

                phases_completed += 1;

                if exit_code != 0 {
                    // Phase failed — retry once
                    warn!(
                        phase = %phase.name,
                        exit_code,
                        "Pilot phase failed, retrying once"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;

                    let retry = run_phase(
                        phase,
                        &config.workdir,
                        config.model.as_deref(),
                        config.effort.as_deref(),
                        mcp_config_path,
                        session_id.as_deref(),
                        &paid,
                        gh_token.as_deref(),
                        cancel_ref,
                        &progress_tx,
                        per_phase_timeout,
                    )
                    .await;

                    match retry {
                        Ok((text, exit_code, it, ot, cost, turns, sid)) => {
                            total_input_tokens += it;
                            total_output_tokens += ot;
                            total_cost += cost;
                            total_turns += turns;
                            last_text = text;
                            last_exit_code = exit_code;
                            if let Some(ref s) = sid {
                                session_id = Some(s.clone());
                            }
                            if exit_code != 0 {
                                warn!(
                                    phase = %phase.name,
                                    exit_code,
                                    "Pilot phase retry also failed, aborting"
                                );
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(
                                phase = %phase.name,
                                error = %e,
                                "Pilot phase retry error, aborting"
                            );
                            last_exit_code = 1;
                            break;
                        }
                    }
                }
            }
            Err(e) if is_cancelled(&e) => {
                return Err(e);
            }
            Err(e) => {
                warn!(
                    phase = %phase.name,
                    error = %e,
                    "Pilot phase error"
                );
                last_exit_code = 1;
                last_text = e.to_string();
                break;
            }
        }
    }

    info!(
        phases_completed,
        phases_total = total_phases,
        total_turns,
        total_cost_usd = format!("{total_cost:.4}"),
        duration_secs = start.elapsed().as_secs(),
        "Claude Pilot execution finished"
    );

    Ok(PilotResult {
        text: last_text,
        exit_code: last_exit_code,
        duration: start.elapsed(),
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        cost_usd: total_cost,
        num_turns: total_turns,
        session_id,
        phases_completed,
        phases_total: total_phases,
    })
}

fn log_phase_event(phase_name: &str, event: &serde_json::Value) {
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match event_type {
        "assistant" => {
            if let Some(tool) = event
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .and_then(|blocks| {
                    blocks.iter().find_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            b.get("name").and_then(|n| n.as_str())
                        } else {
                            None
                        }
                    })
                })
            {
                info!(phase = phase_name, tool, "Claude Pilot tool call");
            }
        }
        "result" => {
            let turns = event.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0);
            let cost = event
                .get("total_cost_usd")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0);
            info!(
                phase = phase_name,
                turns,
                cost_usd = format!("{cost:.4}"),
                "Claude Pilot phase finished"
            );
        }
        _ => {}
    }
}

async fn emit_phase_progress(
    event: &serde_json::Value,
    tx: &Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
    round: &mut u32,
) {
    use workforce_agent::runner::ProgressEvent;
    let Some(tx) = tx else { return };
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "assistant" => {
            *round += 1;
            let _ = tx.send(ProgressEvent::LlmCall { round: *round }).await;

            if let Some(tool) = event
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .and_then(|blocks| {
                    blocks.iter().find_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            b.get("name").and_then(|n| n.as_str())
                        } else {
                            None
                        }
                    })
                })
            {
                let _ = tx
                    .send(ProgressEvent::ToolStart {
                        name: format!("pilot:{tool}"),
                        round: *round,
                        detail: String::new(),
                    })
                    .await;
            }
        }
        "result" => {
            let turns = event.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            let _ = tx
                .send(ProgressEvent::Done {
                    rounds: turns,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    tool_count: 0,
                })
                .await;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_phase_stream_returns_the_result_after_a_megabyte_and_invalid_bytes() {
        let filler =
            serde_json::json!({ "type": "user", "message": { "content": "x".repeat(1_000) } });
        let result = r#"{"type":"result","num_turns":3,"result":"pushed"}"#;
        let stream = [
            format!("{filler}\n").repeat(2_000).as_bytes(),
            b"not \xff utf-8\r\n",
            result.as_bytes(),
        ]
        .concat();

        let event = read_phase_stream(stream.as_slice(), "code", &None)
            .await
            .expect("the result is read");
        assert_eq!(event["result"], "pushed");
        assert_eq!(event["num_turns"], 3);
    }

    #[tokio::test]
    async fn pilot_step_uses_resolved_login() {
        use crate::claude_code::tests::scripted::ScriptedClaude;
        let claude = ScriptedClaude::new();
        let token = format!("sk-ant-oat01-pilot-{}", uuid::Uuid::new_v4().simple());
        let login = crate::subscription::test_support::claude_login(&token);
        let (_service, tools) = crate::mcp_turn::testing::round_tools(
            &["probe.*"],
            crate::mcp_turn::ClientTimeout::CLAUDE_CODE,
        )
        .await;
        let workdir = tempfile::tempdir().unwrap();
        let config = PilotConfig {
            phases: vec![PilotPhase {
                name: "code".into(),
                task: "fix it".into(),
                allowed_tools: vec!["Edit".into()],
                max_turns: 3,
                mcp: true,
            }],
            workdir: workdir.path().to_path_buf(),
            model: None,
            effort: None,
            mcp: Some(tools),
            timeout: Duration::from_secs(60),
            agent_id: "allan".into(),
        };

        let result = claude
            .scoped(run_pilot(&config, &login, None, None, None, None))
            .await
            .unwrap();

        assert_eq!(result.phases_completed, 1);
        let calls = claude.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].token, token,
            "the phase runs on the login resolved for it"
        );
        assert!(calls[0].has("--mcp-config"));
        assert!(
            !calls[0].has("--disallowedTools"),
            "ToolSearch stays on, or no MCP schema loads: {:?}",
            calls[0].args
        );
    }

    #[tokio::test]
    async fn a_pilot_phase_at_its_limit_cools_the_login() {
        use crate::claude_code::tests::scripted::ScriptedClaude;
        let claude = ScriptedClaude::new();
        claude.reply(
            None,
            &[serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": true,
                "num_turns": 1,
                "result": "API Error: Request rejected (429) · rate limited",
            })],
        );
        let login = crate::subscription::test_support::claude_login(&format!(
            "sk-ant-oat01-pilot-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workdir = tempfile::tempdir().unwrap();
        let config = PilotConfig {
            phases: vec![PilotPhase {
                name: "code".into(),
                task: "fix it".into(),
                allowed_tools: Vec::new(),
                max_turns: 3,
                mcp: false,
            }],
            workdir: workdir.path().to_path_buf(),
            model: None,
            effort: None,
            mcp: None,
            timeout: Duration::from_secs(60),
            agent_id: "allan".into(),
        };

        let _ = claude
            .scoped(run_pilot(&config, &login, None, None, None, None))
            .await;

        assert!(
            crate::subscription::CooldownStore::global()
                .until(&login.key, chrono::Utc::now())
                .is_some(),
            "every route that resolves the login skips it while it cools"
        );
    }
}
