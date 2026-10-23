use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use eyre::{eyre, Result, WrapErr};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};
use workforce_common::settings::SecretStore;

use crate::adapters::credential_resolver::resolve_vault_refs;

use super::config::ObscuraSkillConfig;
use super::process::{spawn_mcp, ChildGuard, RespawnBudget};
use super::protocol::{
    matches_id, ClientInfo, IdGen, InitializeParams, Notification, Request, Response,
    ToolsCallParams, METHOD_INITIALIZE, METHOD_NOTIFICATIONS_INITIALIZED, METHOD_TOOLS_CALL,
    PROTOCOL_VERSION,
};

const RESPAWN_WINDOW_MS: u64 = 60_000;
const RESPAWN_LIMIT: usize = 3;
const VAULT_PREFIX: &str = "obscura_creds/";

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

struct Active {
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
    pending: Pending,
    ids: Arc<IdGen>,
    call_lock: Arc<Mutex<()>>,
    _reader_task: JoinHandle<()>,
    _child: ChildGuard,
}

/// Lazy, drop-safe handle to a per-agent Obscura subprocess speaking MCP over
/// stdio. All tool calls are serialised through `call_lock` so the single tab
/// Obscura exposes can't be raced.
pub struct ObscuraClient {
    config: ObscuraSkillConfig,
    inner: Mutex<Option<Active>>,
    respawn_budget: Mutex<RespawnBudget>,
    store: Option<Arc<dyn SecretStore>>,
}

impl ObscuraClient {
    pub fn new(config: ObscuraSkillConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(None),
            respawn_budget: Mutex::new(RespawnBudget::new(RESPAWN_WINDOW_MS, RESPAWN_LIMIT)),
            store: None,
        }
    }

    pub fn with_store(config: ObscuraSkillConfig, store: Arc<dyn SecretStore>) -> Self {
        Self {
            config,
            inner: Mutex::new(None),
            respawn_budget: Mutex::new(RespawnBudget::new(RESPAWN_WINDOW_MS, RESPAWN_LIMIT)),
            store: Some(store),
        }
    }

    fn resolve_credential(&self, raw: &str) -> Result<String> {
        // Obscura runs in a subprocess and cannot observe the destination URL,
        // so it cannot enforce a secret's egress allowlist. Inside a workflow
        // run, refuse every vault marker fail-closed — workflow secrets must
        // flow through the browser sink. Outside a run, the legacy path applies.
        if raw.contains("[[vault:") && workforce_common::secret_policy::in_secret_policy_scope() {
            return Err(eyre!(
                "obscura cannot resolve secret markers in a workflow run; use the browser skill"
            ));
        }
        match self.store.as_ref() {
            Some(store) => resolve_vault_refs(raw, store, VAULT_PREFIX),
            None if raw.contains("[[vault:") => Err(eyre!(
                "obscura tool received `[[vault:...]]` marker but the skill was constructed without a vault store"
            )),
            None => Ok(raw.to_string()),
        }
    }

    async fn ensure_active(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        self.respawn_budget.lock().await.record()?;
        *guard = Some(self.start().await?);
        Ok(())
    }

    async fn start(&self) -> Result<Active> {
        let spawned = spawn_mcp(&self.config)?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let reader_task = {
            let pending = pending.clone();
            tokio::spawn(pump_stdout(spawned.stdout, pending))
        };

        let active = Active {
            writer: Arc::new(Mutex::new(spawned.stdin)),
            pending,
            ids: Arc::new(IdGen::default()),
            call_lock: Arc::new(Mutex::new(())),
            _reader_task: reader_task,
            _child: spawned.guard,
        };

        timeout(
            Duration::from_millis(self.config.startup_timeout_ms),
            handshake(&active),
        )
        .await
        .map_err(|_| eyre!("obscura handshake timed out"))??;

        Ok(active)
    }

    async fn call_method(&self, method: &str, params: Value) -> Result<Value> {
        self.ensure_active().await?;
        let (writer, pending, ids, call_lock) = {
            let guard = self.inner.lock().await;
            let active = guard.as_ref().expect("ensured");
            (
                active.writer.clone(),
                active.pending.clone(),
                active.ids.clone(),
                active.call_lock.clone(),
            )
        };
        let _serial = call_lock.lock().await;
        let id = ids.next();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(id, tx);

        let line = serde_json::to_vec(&Request::new(id, method, params))
            .wrap_err("serialise jsonrpc request")?;
        {
            let mut w = writer.lock().await;
            w.write_all(&line).await.wrap_err("write request")?;
            w.write_all(b"\n").await.wrap_err("write newline")?;
            w.flush().await.wrap_err("flush stdin")?;
        }

        let dur = Duration::from_millis(self.config.call_timeout_ms);
        match timeout(dur, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.recycle().await;
                Err(eyre!("obscura connection closed mid-call"))
            }
            Err(_) => {
                pending.lock().await.remove(&id);
                self.recycle().await;
                Err(eyre!(
                    "obscura tool call timed out after {}ms; subprocess recycled, next call will respawn",
                    dur.as_millis()
                ))
            }
        }
    }

    async fn recycle(&self) {
        warn!(target: "obscura", "tearing down obscura session");
        *self.inner.lock().await = None;
    }

    async fn tools_call(&self, name: &str, arguments: Value) -> Result<Value> {
        let params = serde_json::to_value(ToolsCallParams { name, arguments })
            .wrap_err("serialise tools/call params")?;
        let envelope = self.call_method(METHOD_TOOLS_CALL, params).await?;
        unwrap_mcp_result(envelope)
    }

    pub(super) async fn navigate(&self, url: &str) -> Result<Value> {
        self.tools_call("browser_navigate", json!({ "url": url }))
            .await
    }

    pub(super) async fn snapshot(&self) -> Result<Value> {
        self.tools_call("browser_snapshot", json!({})).await
    }

    pub(super) async fn click(&self, selector: &str) -> Result<Value> {
        self.tools_call("browser_click", json!({ "selector": selector }))
            .await
    }

    pub(super) async fn fill(&self, selector: &str, value: &str) -> Result<Value> {
        let resolved = self.resolve_credential(value)?;
        self.tools_call(
            "browser_fill",
            json!({ "selector": selector, "value": resolved }),
        )
        .await
    }

    pub(super) async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        let resolved = self.resolve_credential(text)?;
        self.tools_call(
            "browser_type",
            json!({ "selector": selector, "text": resolved }),
        )
        .await
    }

    pub(super) async fn press_key(&self, key: &str, selector: Option<&str>) -> Result<Value> {
        let args = match selector {
            Some(sel) => json!({ "key": key, "selector": sel }),
            None => json!({ "key": key }),
        };
        self.tools_call("browser_press_key", args).await
    }

    pub(super) async fn select_option(&self, selector: &str, value: &str) -> Result<Value> {
        self.tools_call(
            "browser_select_option",
            json!({ "selector": selector, "value": value }),
        )
        .await
    }

    pub(super) async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.tools_call("browser_evaluate", json!({ "expression": expression }))
            .await
    }

    pub(super) async fn wait_for_selector(
        &self,
        selector: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value> {
        let args = match timeout_ms {
            Some(t) => json!({ "selector": selector, "timeout": t }),
            None => json!({ "selector": selector }),
        };
        self.tools_call("browser_wait_for", args).await
    }

    pub(super) async fn network_requests(&self) -> Result<Value> {
        self.tools_call("browser_network_requests", json!({})).await
    }

    pub(super) async fn console_messages(&self) -> Result<Value> {
        self.tools_call("browser_console_messages", json!({})).await
    }
}

async fn handshake(active: &Active) -> Result<()> {
    let id = active.ids.next();
    let (tx, rx) = oneshot::channel();
    active.pending.lock().await.insert(id, tx);

    let init = Request::new(
        id,
        METHOD_INITIALIZE,
        InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            capabilities: json!({}),
            client_info: ClientInfo {
                name: "workforce-obscura",
                version: env!("CARGO_PKG_VERSION"),
            },
        },
    );
    let mut line = serde_json::to_vec(&init).wrap_err("serialise initialize")?;
    line.push(b'\n');
    {
        let mut w = active.writer.lock().await;
        w.write_all(&line).await.wrap_err("write initialize")?;
        w.flush().await.wrap_err("flush initialize")?;
    }
    rx.await
        .map_err(|_| eyre!("obscura closed during handshake"))??;

    let initialized = Notification::new(METHOD_NOTIFICATIONS_INITIALIZED, json!({}));
    let mut line = serde_json::to_vec(&initialized).wrap_err("serialise initialized")?;
    line.push(b'\n');
    let mut w = active.writer.lock().await;
    w.write_all(&line).await.wrap_err("write initialized")?;
    w.flush().await.wrap_err("flush initialized")?;
    Ok(())
}

async fn pump_stdout(
    mut stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    pending: Pending,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) => {
                fail_all_pending(&pending, "obscura stdout closed").await;
                return;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Response>(trimmed) {
                    Ok(resp) => route_response(&pending, resp).await,
                    Err(e) => {
                        debug!(target: "obscura", error = %e, raw = %trimmed, "non-jsonrpc line on stdout");
                    }
                }
            }
            Err(e) => {
                warn!(target: "obscura", %e, "stdout read error");
                fail_all_pending(&pending, "obscura stdout read error").await;
                return;
            }
        }
    }
}

async fn route_response(pending: &Pending, resp: Response) {
    let Some(id_value) = resp.id else {
        return;
    };
    let mut map = pending.lock().await;
    let id = match id_value.as_u64() {
        Some(n) => n,
        None => match id_value.as_str().and_then(|s| s.parse::<u64>().ok()) {
            Some(n) => n,
            None => return,
        },
    };
    if !map.keys().any(|k| matches_id(&json!(*k), id)) {
        return;
    }
    if let Some(tx) = map.remove(&id) {
        let outcome = match (resp.result, resp.error) {
            (Some(r), None) => Ok(r),
            (_, Some(err)) => Err(eyre!("{err}")),
            (None, None) => Err(eyre!("jsonrpc response with neither result nor error")),
        };
        let _ = tx.send(outcome);
    }
}

async fn fail_all_pending(pending: &Pending, reason: &str) {
    let mut map = pending.lock().await;
    for (_id, tx) in map.drain() {
        let _ = tx.send(Err(eyre!("{reason}")));
    }
}

/// Unwrap the MCP `tools/call` envelope (`{content:[{type,text}], isError?}`)
/// into the agent-facing payload. `isError: true` becomes a tool failure
/// carrying the text from the first content item. Otherwise the joined text
/// is returned as a JSON value when it parses, else as a plain string.
fn unwrap_mcp_result(envelope: Value) -> Result<Value> {
    let texts: Vec<String> = envelope
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let joined = texts.join("\n");
    let is_error = envelope
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        let message = if joined.is_empty() {
            "obscura tool returned isError without content".to_string()
        } else {
            joined
        };
        return Err(eyre!("{message}"));
    }
    if joined.is_empty() {
        return Ok(envelope);
    }
    match serde_json::from_str::<Value>(&joined) {
        Ok(v) => Ok(v),
        Err(_) => Ok(json!({ "text": joined })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_prefix_isolated_from_browser() {
        assert_eq!(VAULT_PREFIX, "obscura_creds/");
        assert_ne!(VAULT_PREFIX, "browser_creds/");
    }

    #[tokio::test]
    async fn resolve_credential_passes_through_literal() {
        let client = ObscuraClient::new(ObscuraSkillConfig::default());
        assert_eq!(
            client.resolve_credential("plainvalue").unwrap(),
            "plainvalue"
        );
    }

    #[tokio::test]
    async fn resolve_credential_rejects_vault_ref_without_store() {
        let client = ObscuraClient::new(ObscuraSkillConfig::default());
        let err = client
            .resolve_credential("[[vault:foo]]")
            .expect_err("expected error");
        assert!(format!("{err}").contains("without a vault store"));
    }

    #[test]
    fn unwrap_mcp_result_parses_json_text() {
        let envelope = json!({
            "content": [{"type": "text", "text": "{\"url\":\"x\",\"title\":\"t\"}"}]
        });
        let out = unwrap_mcp_result(envelope).unwrap();
        assert_eq!(out["url"], "x");
        assert_eq!(out["title"], "t");
    }

    #[test]
    fn unwrap_mcp_result_wraps_non_json_text() {
        let envelope = json!({
            "content": [{"type": "text", "text": "plain status string"}]
        });
        let out = unwrap_mcp_result(envelope).unwrap();
        assert_eq!(out["text"], "plain status string");
    }

    #[test]
    fn unwrap_mcp_result_joins_multiple_text_items() {
        let envelope = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        });
        let out = unwrap_mcp_result(envelope).unwrap();
        assert_eq!(out["text"], "line one\nline two");
    }

    #[test]
    fn unwrap_mcp_result_returns_err_on_is_error_true() {
        let envelope = json!({
            "content": [{"type": "text", "text": "Network error: forbidden"}],
            "isError": true
        });
        let err = unwrap_mcp_result(envelope).expect_err("expected error");
        assert!(format!("{err}").contains("Network error"));
    }

    #[test]
    fn unwrap_mcp_result_falls_through_when_no_content() {
        let envelope = json!({ "ok": true });
        let out = unwrap_mcp_result(envelope.clone()).unwrap();
        assert_eq!(out, envelope);
    }
}
