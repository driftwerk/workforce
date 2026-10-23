//! Local agent — connects to a gateway and executes commands on the local machine.
//!
//! The local agent establishes a WebSocket connection to the gateway, registers
//! itself with a `LocalAgentHello` message, then listens for `local_exec` commands.
//! Each command is executed as a child process with stdout/stderr streamed back as
//! `local_exec_progress` messages. Upon completion (or timeout / cancellation),
//! a `local_exec_result` message is sent.
//!
//! The agent auto-reconnects with exponential back-off if the connection drops.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

/// Maximum reconnect back-off delay.
const MAX_BACKOFF_SECS: u64 = 30;

/// Run the local agent, reconnecting on failure with exponential back-off.
pub async fn run_local_agent(
    gateway_url: &str,
    token: &str,
    agent_id: &str,
    name: &str,
    workdir: Option<PathBuf>,
) -> Result<()> {
    let effective_workdir = workdir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut backoff_secs: u64 = 1;

    loop {
        info!(
            gateway = %gateway_url,
            agent_id = %agent_id,
            workdir = %effective_workdir.display(),
            "Connecting to gateway..."
        );

        match connect_and_run(gateway_url, token, agent_id, name, &effective_workdir).await {
            Ok(()) => {
                info!("Disconnected cleanly");
                break;
            }
            Err(e) => {
                warn!(error = %e, backoff_secs, "Connection lost — reconnecting");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                // Exponential back-off capped at MAX_BACKOFF_SECS.
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            }
        }
    }

    Ok(())
}

/// Single connection lifecycle: connect, register, process messages, return on
/// disconnect or error.
async fn connect_and_run(
    gateway_url: &str,
    token: &str,
    agent_id: &str,
    name: &str,
    workdir: &Path,
) -> Result<()> {
    let ws_url = build_ws_url(gateway_url, token)?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url).await?;
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    info!("WebSocket connected — sending hello");

    // ── Registration ────────────────────────────────────────────────────
    let hello = serde_json::json!({
        "type": "local_agent_hello",
        "agent_node_id": agent_id,
        "name": name,
        "capabilities": ["shell", "filesystem"],
        "allowed_paths": [workdir.to_string_lossy()],
    });
    ws_sink
        .send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;

    // ── Shared state ────────────────────────────────────────────────────
    // Map of request_id -> cancel sender, for in-flight processes.
    let running: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<()>>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Outbound message channel — all tasks send through here.
    let (send_tx, mut send_rx) = mpsc::channel::<String>(64);

    // ── Write loop (mpsc -> WebSocket) ──────────────────────────────────
    let write_handle = tokio::spawn(async move {
        while let Some(msg) = send_rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // ── Read loop (WebSocket -> dispatch) ───────────────────────────────
    while let Some(msg_result) = ws_stream.next().await {
        let msg = msg_result?;
        match msg {
            Message::Text(text) => {
                let value: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "Received non-JSON message — ignoring");
                        continue;
                    }
                };

                let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match msg_type {
                    // ── Execute a command ────────────────────────────
                    "local_exec" => {
                        let request_id = value["request_id"].as_str().unwrap_or("").to_string();
                        let command = value["command"].as_str().unwrap_or("").to_string();
                        let cmd_workdir = value["workdir"]
                            .as_str()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| workdir.to_path_buf());
                        let timeout_secs = value["timeout_secs"].as_u64().unwrap_or(300);

                        let send_tx = send_tx.clone();
                        let running = Arc::clone(&running);

                        info!(
                            request_id = %request_id,
                            command = %command,
                            workdir = %cmd_workdir.display(),
                            timeout_secs,
                            "Executing command"
                        );

                        // Create a one-shot channel for cancellation.
                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                        running.write().await.insert(request_id.clone(), cancel_tx);

                        tokio::spawn(async move {
                            execute_command(
                                &request_id,
                                &command,
                                &cmd_workdir,
                                timeout_secs,
                                cancel_rx,
                                &send_tx,
                            )
                            .await;
                            running.write().await.remove(&request_id);
                        });
                    }

                    // ── Cancel a running command ─────────────────────
                    "local_exec_cancel" => {
                        let request_id = value["request_id"].as_str().unwrap_or("").to_string();
                        info!(request_id = %request_id, "Cancel requested");

                        if let Some(cancel_tx) = running.write().await.remove(&request_id) {
                            let _ = cancel_tx.send(());
                        } else {
                            warn!(
                                request_id = %request_id,
                                "Cancel requested but no running process found"
                            );
                        }
                    }

                    // ── Keepalive ────────────────────────────────────
                    "ping" => {
                        let pong = serde_json::json!({"type": "pong"});
                        let _ = send_tx.send(serde_json::to_string(&pong).unwrap()).await;
                    }

                    other => {
                        if !other.is_empty() {
                            info!(msg_type = %other, "Ignoring unknown message type");
                        }
                    }
                }
            }
            Message::Ping(payload) => {
                // Respond to WebSocket-level pings.
                let _ = send_tx
                    .send(serde_json::to_string(&serde_json::json!({"type": "pong"})).unwrap())
                    .await;
                // Note: tungstenite auto-responds to WS pings, but we also handle
                // application-level pong here for completeness.
                let _ = payload; // suppress unused warning
            }
            Message::Close(_) => {
                info!("Received WebSocket close frame");
                break;
            }
            _ => {}
        }
    }

    write_handle.abort();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Command execution
// ─────────────────────────────────────────────────────────────────────────────

/// Execute a shell command, streaming output and sending the final result.
async fn execute_command(
    request_id: &str,
    command: &str,
    workdir: &Path,
    timeout_secs: u64,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
    send_tx: &mpsc::Sender<String>,
) {
    // Spawn the child process.
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            error!(request_id = %request_id, error = %e, "Failed to spawn command");
            let result = serde_json::json!({
                "type": "local_exec_result",
                "request_id": request_id,
                "ok": false,
                "error": format!("Failed to spawn command: {}", e),
            });
            let _ = send_tx.send(serde_json::to_string(&result).unwrap()).await;
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // ── Stream stdout ───────────────────────────────────────────────────
    let req_id_stdout = request_id.to_string();
    let tx_stdout = send_tx.clone();
    let stdout_handle = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            let progress = serde_json::json!({
                "type": "local_exec_progress",
                "request_id": &req_id_stdout,
                "stream": "stdout",
                "data": &line,
            });
            let _ = tx_stdout
                .send(serde_json::to_string(&progress).unwrap())
                .await;
        }
        collected
    });

    // ── Stream stderr ───────────────────────────────────────────────────
    let req_id_stderr = request_id.to_string();
    let tx_stderr = send_tx.clone();
    let stderr_handle = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            let progress = serde_json::json!({
                "type": "local_exec_progress",
                "request_id": &req_id_stderr,
                "stream": "stderr",
                "data": &line,
            });
            let _ = tx_stderr
                .send(serde_json::to_string(&progress).unwrap())
                .await;
        }
        collected
    });

    // ── Wait for completion, timeout, or cancellation ───────────────────
    let timeout = Duration::from_secs(timeout_secs);

    tokio::select! {
        // Normal completion.
        status = child.wait() => {
            let stdout_text = stdout_handle.await.unwrap_or_default();
            let stderr_text = stderr_handle.await.unwrap_or_default();

            let (ok, exit_code) = match status {
                Ok(s) => (s.success(), s.code()),
                Err(e) => {
                    error!(request_id = %request_id, error = %e, "Process wait error");
                    let result = serde_json::json!({
                        "type": "local_exec_result",
                        "request_id": request_id,
                        "ok": false,
                        "error": format!("Process error: {}", e),
                    });
                    let _ = send_tx.send(serde_json::to_string(&result).unwrap()).await;
                    return;
                }
            };

            info!(
                request_id = %request_id,
                ok,
                exit_code = ?exit_code,
                "Command completed"
            );

            // Include an error summary when the command fails so the hub
            // doesn't fall back to the generic "Unknown local agent error".
            let error = if !ok {
                let msg = if !stderr_text.is_empty() {
                    stderr_text.trim().to_string()
                } else {
                    format!(
                        "Command exited with code {}",
                        exit_code.unwrap_or(-1)
                    )
                };
                Some(msg)
            } else {
                None
            };

            let result = serde_json::json!({
                "type": "local_exec_result",
                "request_id": request_id,
                "ok": ok,
                "stdout": stdout_text,
                "stderr": stderr_text,
                "exit_code": exit_code,
                "error": error,
            });
            let _ = send_tx.send(serde_json::to_string(&result).unwrap()).await;
        }

        // Timeout.
        _ = tokio::time::sleep(timeout) => {
            warn!(request_id = %request_id, timeout_secs, "Command timed out — killing");
            let _ = child.kill().await;
            // Wait for stream tasks to finish after kill.
            let _ = stdout_handle.await;
            let _ = stderr_handle.await;

            let result = serde_json::json!({
                "type": "local_exec_result",
                "request_id": request_id,
                "ok": false,
                "error": format!("Command timed out after {}s", timeout_secs),
            });
            let _ = send_tx.send(serde_json::to_string(&result).unwrap()).await;
        }

        // Cancellation.
        _ = cancel_rx => {
            info!(request_id = %request_id, "Command cancelled — killing");
            let _ = child.kill().await;
            // Wait for stream tasks to finish after kill.
            let _ = stdout_handle.await;
            let _ = stderr_handle.await;

            let result = serde_json::json!({
                "type": "local_exec_result",
                "request_id": request_id,
                "ok": false,
                "error": "Cancelled by user",
            });
            let _ = send_tx.send(serde_json::to_string(&result).unwrap()).await;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Build the WebSocket URL from the gateway base URL and auth token.
///
/// Handles `http://`, `https://`, `ws://`, `wss://`, and bare `host:port` forms.
fn build_ws_url(gateway_url: &str, token: &str) -> Result<String> {
    let mut url = gateway_url.to_string();

    // Normalise scheme to ws/wss.
    if url.starts_with("http://") {
        url = format!("ws://{}", &url[7..]);
    } else if url.starts_with("https://") {
        url = format!("wss://{}", &url[8..]);
    } else if !url.starts_with("ws://") && !url.starts_with("wss://") {
        url = format!("ws://{}", url);
    }

    // Strip trailing slash.
    let url = url.trim_end_matches('/');

    // Append path and auth token.
    Ok(format!("{}/ws/agent?token={}", url, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ws_url_http() {
        let url = build_ws_url("http://localhost:8091", "tok123").unwrap();
        assert_eq!(url, "ws://localhost:8091/ws/agent?token=tok123");
    }

    #[test]
    fn test_build_ws_url_https() {
        let url = build_ws_url("https://gateway.example.com", "abc").unwrap();
        assert_eq!(url, "wss://gateway.example.com/ws/agent?token=abc");
    }

    #[test]
    fn test_build_ws_url_ws() {
        let url = build_ws_url("ws://10.0.0.1:9000", "x").unwrap();
        assert_eq!(url, "ws://10.0.0.1:9000/ws/agent?token=x");
    }

    #[test]
    fn test_build_ws_url_bare() {
        let url = build_ws_url("myhost:8091", "t").unwrap();
        assert_eq!(url, "ws://myhost:8091/ws/agent?token=t");
    }

    #[test]
    fn test_build_ws_url_trailing_slash() {
        let url = build_ws_url("http://localhost:8091/", "t").unwrap();
        assert_eq!(url, "ws://localhost:8091/ws/agent?token=t");
    }
}
