//! Local agent — connects to a gateway and executes commands on the local machine.
//!
//! The local agent establishes a WebSocket connection to the gateway, registers
//! itself with a `LocalAgentHello` message, then listens for `local_exec` commands.
//! Each command is executed as a child process with stdout/stderr streamed back as
//! `local_exec_progress` messages. Upon completion (or timeout / cancellation),
//! a `local_exec_result` message is sent.
//!
//! With `--mcp`, it also tunnels an MCP server on this machine: the tools that
//! server lists (those `--mcp-tools` names) are offered to the remote hub in
//! the hello, re-offered when they change, and each `node_tool_call` is run on
//! that server and answered with a `node_tool_result`.
//!
//! The agent auto-reconnects with exponential back-off if the connection drops,
//! and stops when the gateway refuses it.

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
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use workforce_gateway_client::protocol::{
    LocalAgentHello, LocalAgentRefused, NodeToolCall, NodeToolCancel, NodeToolResult, NodeToolSpec,
    NodeTools,
};
use workforce_mcp_client::{McpClient, McpLimits, McpToolDef, StaticBearer};

/// Maximum reconnect back-off delay.
const MAX_BACKOFF_SECS: u64 = 30;

/// Where `--mcp`'s bearer is read from, so it never appears on a command line.
pub const MCP_TOKEN_ENV: &str = "WORKFORCE_NODE_MCP_TOKEN";

/// How often a tunnelled MCP server's tools are listed again, to offer the
/// hub what changed.
const TOOLS_REFRESH: Duration = Duration::from_secs(60);

/// The tools a tunnel never offers: the approval bridge of a workforce hub's
/// MCP endpoint, which answers the prompts that endpoint's own calls raise.
/// A remote agent holding it could approve its own calls on this machine;
/// those prompts are the person's, in Desk.
const NEVER_OFFERED: &str = "approval_";

/// The gateway refused this agent; connecting again would be refused too.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the gateway refused this local agent: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// An MCP server on this machine whose tools the agent offers the hub.
pub struct McpTunnel {
    client: McpClient,
    /// Whether the server has been initialised, which it is before the first
    /// listing that reaches it.
    initialized: std::sync::atomic::AtomicBool,
    /// Which of the server's tools to offer: exact names, or prefixes ending
    /// in `*`.
    patterns: Vec<String>,
}

impl McpTunnel {
    /// A tunnel to the streamable-HTTP MCP endpoint `url`, authenticated
    /// with `bearer`, offering the tools `patterns` name.
    pub fn new(url: &str, bearer: String, patterns: Vec<String>) -> Result<Self> {
        let client = McpClient::new(
            reqwest::Url::parse(url)?,
            Arc::new(reqwest::Client::new()),
            Arc::new(StaticBearer::new(bearer)),
            McpLimits::default(),
        );
        Ok(Self {
            client,
            initialized: Default::default(),
            patterns,
        })
    }

    /// The tools to offer now, as the server lists them.
    async fn tools(&self) -> Result<Vec<NodeToolSpec>> {
        use std::sync::atomic::Ordering;
        if !self.initialized.load(Ordering::Acquire) {
            self.client.initialize().await?;
            self.initialized.store(true, Ordering::Release);
        }
        let listed = self
            .client
            .tools_list_filtered(|name| offers(&self.patterns, name))
            .await?;
        Ok(listed.into_iter().map(spec).collect())
    }

    /// Run `call` on the server; `cancel` abandons it.
    async fn call(&self, call: NodeToolCall, cancel: CancellationToken) -> NodeToolResult {
        let outcome = self
            .client
            .tools_call(&call.name, call.arguments, cancel)
            .await;
        NodeToolResult {
            msg_type: "node_tool_result".to_string(),
            request_id: call.request_id,
            error: outcome.as_ref().err().map(ToString::to_string),
            result: outcome.ok(),
        }
    }
}

/// Whether a tool named `name` is offered under `patterns`.
fn offers(patterns: &[String], name: &str) -> bool {
    !name.starts_with(NEVER_OFFERED)
        && patterns
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => pattern == name,
            })
}

fn spec(tool: McpToolDef) -> NodeToolSpec {
    NodeToolSpec {
        name: tool.name,
        description: tool.description,
        input_schema: tool.input_schema,
        read_only: tool.annotations.read_only_hint,
    }
}

/// The node id this machine goes by when none is given: its host name's
/// first label, as a hub accepts a node id.
pub fn default_node_id() -> String {
    let host = hostname::get()
        .map(|h| h.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    node_id_from(&host)
}

fn node_id_from(host: &str) -> String {
    let label: String = host
        .split('.')
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' => c,
            _ => '-',
        })
        .take(workforce_hub::local_agent_registry::MAX_NODE_ID_LEN)
        .collect();
    match label.trim_matches('-') {
        "" => "local-agent".to_string(),
        id => id.to_string(),
    }
}

/// Run the local agent, reconnecting on failure with exponential back-off.
pub async fn run_local_agent(
    gateway_url: &str,
    token: &str,
    agent_id: &str,
    name: &str,
    workdir: Option<PathBuf>,
    mcp: Option<McpTunnel>,
) -> Result<()> {
    let effective_workdir = workdir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mcp = mcp.map(Arc::new);
    let mut backoff_secs: u64 = 1;

    loop {
        info!(
            gateway = %gateway_url,
            agent_id = %agent_id,
            workdir = %effective_workdir.display(),
            "Connecting to gateway..."
        );

        match connect_and_run(
            gateway_url,
            token,
            agent_id,
            name,
            &effective_workdir,
            mcp.as_ref(),
        )
        .await
        {
            Ok(()) => {
                info!("Disconnected cleanly");
                break;
            }
            Err(e) if e.downcast_ref::<Refused>().is_some() => return Err(e),
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
    mcp: Option<&Arc<McpTunnel>>,
) -> Result<()> {
    let ws_url = build_ws_url(gateway_url, token)?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url).await?;
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    info!("WebSocket connected — sending hello");

    // ── Registration ────────────────────────────────────────────────────
    // A server not answering yet leaves `exec.run` working: its tools are
    // offered by the next refresh that reaches it.
    let mut offered = match mcp {
        Some(mcp) => mcp.tools().await.unwrap_or_else(|e| {
            warn!(error = %e, "Could not list the tunnelled MCP server's tools");
            Vec::new()
        }),
        None => Vec::new(),
    };
    info!(tools = offered.len(), "Offering tools");
    let hello = LocalAgentHello {
        msg_type: "local_agent_hello".to_string(),
        agent_node_id: agent_id.to_string(),
        name: name.to_string(),
        capabilities: vec!["shell".into(), "filesystem".into()],
        allowed_paths: vec![workdir.to_string_lossy().into_owned()],
        tools: offered.clone(),
    };
    ws_sink
        .send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;

    // ── Shared state ────────────────────────────────────────────────────
    // Map of request_id -> cancel sender, for in-flight processes.
    let running: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<()>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    // Map of request_id -> cancellation, for in-flight tool calls.
    let calls: Arc<parking_lot::Mutex<HashMap<String, CancellationToken>>> = Arc::default();

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

    let mut refresh =
        tokio::time::interval_at(tokio::time::Instant::now() + TOOLS_REFRESH, TOOLS_REFRESH);

    // ── Read loop (WebSocket -> dispatch) ───────────────────────────────
    loop {
        let msg = tokio::select! {
            msg = ws_stream.next() => match msg {
                Some(msg) => msg?,
                None => break,
            },
            _ = refresh.tick(), if mcp.is_some() => {
                let mcp = mcp.expect("refreshes only with a tunnel");
                match mcp.tools().await {
                    Ok(tools) if tools != offered => {
                        info!(tools = tools.len(), "Offering changed tools");
                        let update = NodeTools {
                            msg_type: "node_tools".to_string(),
                            tools: tools.clone(),
                        };
                        let _ = send_tx.send(serde_json::to_string(&update)?).await;
                        offered = tools;
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "Could not list the tunnelled MCP server's tools"),
                }
                continue;
            }
        };
        match msg {
            Message::Text(text) => {
                let value: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "Received non-JSON message — ignoring");
                        continue;
                    }
                };

                let msg_type = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                match msg_type.as_str() {
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

                    // ── Run a tunnelled MCP tool ─────────────────────
                    "node_tool_call" => {
                        let call: NodeToolCall = match serde_json::from_value(value) {
                            Ok(call) => call,
                            Err(e) => {
                                warn!(error = %e, "Malformed node_tool_call — ignoring");
                                continue;
                            }
                        };
                        let Some(mcp) = mcp.cloned() else {
                            let refused = NodeToolResult {
                                msg_type: "node_tool_result".to_string(),
                                request_id: call.request_id,
                                result: None,
                                error: Some("this local agent tunnels no MCP server".into()),
                            };
                            let _ = send_tx.send(serde_json::to_string(&refused)?).await;
                            continue;
                        };
                        info!(request_id = %call.request_id, tool = %call.name, "Running tool");
                        let cancel = CancellationToken::new();
                        calls.lock().insert(call.request_id.clone(), cancel.clone());
                        let (send_tx, calls) = (send_tx.clone(), Arc::clone(&calls));
                        tokio::spawn(async move {
                            let request_id = call.request_id.clone();
                            let result = mcp.call(call, cancel).await;
                            calls.lock().remove(&request_id);
                            if let Ok(json) = serde_json::to_string(&result) {
                                let _ = send_tx.send(json).await;
                            }
                        });
                    }

                    // ── Abandon a tool call ──────────────────────────
                    "node_tool_cancel" => {
                        if let Ok(cancel) = serde_json::from_value::<NodeToolCancel>(value) {
                            if let Some(token) = calls.lock().remove(&cancel.request_id) {
                                token.cancel();
                            }
                        }
                    }

                    // ── Refused: connecting again is refused too ─────
                    "local_agent_refused" => {
                        let refused: LocalAgentRefused = serde_json::from_value(value)?;
                        write_handle.abort();
                        return Err(Refused(refused.message).into());
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
    fn a_tunnel_offers_what_its_patterns_name_and_never_the_approval_bridge() {
        let patterns = ["desk_*".to_string(), "filesystem_read".to_string()];
        for offered in ["desk_sessions", "desk_start", "filesystem_read"] {
            assert!(offers(&patterns, offered), "{offered}");
        }
        for withheld in ["filesystem_write", "vault_get", "desk"] {
            assert!(!offers(&patterns, withheld), "{withheld}");
        }
        let everything = ["*".to_string()];
        assert!(offers(&everything, "agent_chat"));
        assert!(!offers(&everything, "approval_decide"));
        assert!(!offers(&everything, "approval_status"));
    }

    #[test]
    fn a_read_only_hint_marks_the_offered_tool() {
        let listed: McpToolDef = serde_json::from_value(serde_json::json!({
            "name": "desk_sessions",
            "description": "List sessions",
            "inputSchema": {"type": "object"},
            "annotations": {"readOnlyHint": true},
        }))
        .unwrap();
        let offered = spec(listed);
        assert!(offered.read_only);
        assert_eq!(offered.name, "desk_sessions");
        let unmarked: McpToolDef =
            serde_json::from_value(serde_json::json!({"name": "desk_start"})).unwrap();
        assert!(!spec(unmarked).read_only);
    }

    #[test]
    fn the_default_node_id_is_the_host_names_first_label_as_a_hub_accepts_it() {
        for (host, id) in [
            ("kingsleys-macbook-pro.local", "kingsleys-macbook-pro"),
            ("Studio_2", "studio-2"),
            (
                "a-very-long-host-name-for-a-mac-mini",
                "a-very-long-host-name-fo",
            ),
            ("...", "local-agent"),
        ] {
            let derived = node_id_from(&host.to_lowercase());
            assert_eq!(derived, id, "{host}");
            assert!(workforce_hub::local_agent_registry::valid_node_id(&derived));
        }
    }

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
