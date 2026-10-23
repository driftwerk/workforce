//! Remote workforce hubs behind the `wf.*` tools.
//!
//! One hub is active at a time. Its remote tools are advertised as
//! `wf.<remote_name>` and forwarded verbatim over the hub's streamable-HTTP
//! MCP endpoint with the user's bearer token. Switching hubs (`wf.use`)
//! reconnects and swaps the advertised set without a restart.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use rmcp::model::{JsonObject, Tool as RmcpTool};
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use workforce_agent::llm::tool_schema::sanitize_tool_schema;
use workforce_common::settings::SecretStore;
use workforce_mcp_client::{McpClient, McpLimits, McpToolDef, StaticBearer};

use crate::hubs::{self, HubEntry};

pub const REMOTE_PREFIX: &str = "wf.";

/// Bound on `initialize` + `tools/list` when (re)connecting, so a hung hub
/// can never wedge startup or a `wf.use` switch.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[async_trait]
pub trait HubSession: Send + Sync {
    fn tools(&self) -> &[McpToolDef];
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String>;
}

#[async_trait]
pub trait HubConnector: Send + Sync {
    async fn connect(&self, hub: &HubEntry) -> Result<Arc<dyn HubSession>>;
}

pub struct HttpConnector {
    http: Arc<reqwest::Client>,
}

impl Default for HttpConnector {
    fn default() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            http: Arc::new(http),
        }
    }
}

struct HttpSession {
    client: McpClient,
    tools: Vec<McpToolDef>,
}

#[async_trait]
impl HubConnector for HttpConnector {
    async fn connect(&self, hub: &HubEntry) -> Result<Arc<dyn HubSession>> {
        let endpoint = url::Url::parse(&hub.mcp_endpoint())
            .wrap_err_with(|| format!("hub {} has an invalid URL", hub.name))?;
        let client = McpClient::new(
            endpoint,
            Arc::clone(&self.http),
            Arc::new(StaticBearer::new(hub.token.clone())),
            McpLimits::default(),
        );
        client
            .initialize()
            .await
            .wrap_err_with(|| format!("initialising MCP session with hub {}", hub.name))?;
        let tools = client
            .tools_list()
            .await
            .wrap_err_with(|| format!("listing tools on hub {}", hub.name))?;
        Ok(Arc::new(HttpSession { client, tools }))
    }
}

#[async_trait]
impl HubSession for HttpSession {
    fn tools(&self) -> &[McpToolDef] {
        &self.tools
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value, String> {
        self.client
            .tools_call(name, arguments, cancel)
            .await
            .map_err(|e| e.to_string())
    }
}

struct Link {
    session: Arc<dyn HubSession>,
    tools: Arc<Vec<RmcpTool>>,
}

struct ActiveHub {
    name: String,
    link: Result<Link, String>,
}

pub struct RemoteHubs {
    store: Box<dyn SecretStore>,
    connector: Arc<dyn HubConnector>,
    active: RwLock<Option<ActiveHub>>,
    settled: watch::Sender<bool>,
}

#[derive(Debug, serde::Serialize)]
pub struct HubView {
    pub name: String,
    pub url: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActiveState {
    Connected { tool_count: usize },
    Disconnected { error: String },
}

#[derive(Debug, serde::Serialize)]
pub struct RemoteStatus {
    pub hubs: Vec<HubView>,
    pub active: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<ActiveState>,
}

impl RemoteHubs {
    pub fn new(store: impl SecretStore + 'static, connector: Arc<dyn HubConnector>) -> Self {
        Self {
            store: Box::new(store),
            connector,
            active: RwLock::new(None),
            settled: watch::channel(false).0,
        }
    }

    /// Connects to the persisted active hub. A failure leaves the hub active
    /// but disconnected, so `wf.hubs` can explain why no `wf.*` tools exist.
    pub async fn connect_active(&self) {
        match hubs::active_hub(self.store.as_ref()) {
            Ok(Some(hub)) => {
                self.activate(hub).await;
            }
            Ok(None) => info!("no active workforce hub; wf.* tools are not advertised"),
            Err(e) => warn!(error = %e, "could not read the active workforce hub"),
        }
        self.settled.send_replace(true);
    }

    /// Runs [`Self::connect_active`] off the startup path; [`Self::wait_ready`]
    /// lets the first `tools/list` wait for it without blocking `initialize`.
    pub fn spawn_connect_active(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.connect_active().await });
    }

    /// Waits until the startup connection attempt has settled, or `limit`.
    pub async fn wait_ready(&self, limit: Duration) {
        let mut settled = self.settled.subscribe();
        let _ = tokio::time::timeout(limit, settled.wait_for(|done| *done)).await;
    }

    /// Makes `name` the persisted active hub and connects to it. Errors only
    /// when `name` is unknown, before anything changes; an unreachable hub is
    /// reported as [`ActiveState::Disconnected`] and is still the active one.
    pub async fn switch(&self, name: &str) -> Result<ActiveState> {
        hubs::set_active_hub(self.store.as_ref(), name)?;
        let hub = hubs::get_hub(self.store.as_ref(), name)?
            .ok_or_else(|| eyre::eyre!("hub {name:?} has no stored entry"))?;
        Ok(self.activate(hub).await)
    }

    async fn activate(&self, hub: HubEntry) -> ActiveState {
        let name = hub.name.clone();
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect(&hub))
            .await
            .unwrap_or_else(|_| {
                Err(eyre::eyre!(
                    "timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                ))
            });
        let link = match connected {
            Ok(session) => {
                let tools: Vec<RmcpTool> = session.tools().iter().map(rmcp_tool_for).collect();
                info!(hub = %name, tools = tools.len(), "connected to workforce hub");
                Ok(Link {
                    session,
                    tools: Arc::new(tools),
                })
            }
            Err(e) => {
                warn!(hub = %name, error = %e, "could not connect to workforce hub; wf.* tools unavailable");
                Err(e.to_string())
            }
        };
        let state = link_state(&link);
        *self.active.write().unwrap() = Some(ActiveHub { name, link });
        state
    }

    fn link_tools(&self) -> Option<Arc<Vec<RmcpTool>>> {
        self.active
            .read()
            .unwrap()
            .as_ref()
            .and_then(|active| active.link.as_ref().ok())
            .map(|link| Arc::clone(&link.tools))
    }

    pub fn tools(&self) -> Vec<RmcpTool> {
        self.link_tools()
            .map(|tools| tools.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn tool(&self, name: &str) -> Option<RmcpTool> {
        self.link_tools()?
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    /// Forwards a `wf.*` call to the active hub and returns the text content.
    pub async fn call(
        &self,
        name: &str,
        arguments: JsonObject,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let remote_name = name
            .strip_prefix(REMOTE_PREFIX)
            .ok_or_else(|| format!("'{name}' is not a remote workforce tool"))?;
        let session = {
            let guard = self.active.read().unwrap();
            match guard.as_ref() {
                Some(ActiveHub { link: Ok(link), .. }) => Arc::clone(&link.session),
                Some(ActiveHub { name, link: Err(e) }) => {
                    return Err(format!("workforce hub {name:?} is unreachable: {e}"));
                }
                None => {
                    return Err(
                        "no active workforce hub; register one with `workforce-local-mcp config workforce add`"
                            .to_string(),
                    );
                }
            }
        };
        let result = session
            .call(remote_name, Value::Object(arguments), cancel)
            .await?;
        call_result_text(result)
    }

    pub fn status(&self) -> RemoteStatus {
        let active = self.active.read().unwrap();
        let active_name = active.as_ref().map(|a| a.name.clone());
        let hubs = hubs::list_hubs(self.store.as_ref())
            .unwrap_or_default()
            .into_iter()
            .map(|hub| HubView {
                active: active_name.as_deref() == Some(hub.name.as_str()),
                name: hub.name,
                url: hub.url,
            })
            .collect();
        RemoteStatus {
            hubs,
            active: active_name,
            connection: active.as_ref().map(|a| link_state(&a.link)),
        }
    }
}

fn link_state(link: &Result<Link, String>) -> ActiveState {
    match link {
        Ok(link) => ActiveState::Connected {
            tool_count: link.tools.len(),
        },
        Err(error) => ActiveState::Disconnected {
            error: error.clone(),
        },
    }
}

pub fn remote_tool_name(remote: &str) -> String {
    format!("{REMOTE_PREFIX}{remote}")
}

fn rmcp_tool_for(def: &McpToolDef) -> RmcpTool {
    let schema = match sanitize_tool_schema(def.input_schema.clone()) {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    RmcpTool::new(
        remote_tool_name(&def.name),
        def.description.clone(),
        Arc::new(schema),
    )
}

/// Flattens an MCP `CallToolResult` to its text; `isError` becomes `Err`.
fn call_result_text(result: Value) -> Result<String, String> {
    let text = result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if result["isError"].as_bool().unwrap_or(false) {
        Err(if text.is_empty() {
            "remote tool failed".to_string()
        } else {
            text
        })
    } else {
        Ok(text)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    pub struct FakeSession {
        pub tools: Vec<McpToolDef>,
        pub calls: Mutex<Vec<(String, Value)>>,
        pub reply: Value,
    }

    #[async_trait]
    impl HubSession for FakeSession {
        fn tools(&self) -> &[McpToolDef] {
            &self.tools
        }
        async fn call(
            &self,
            name: &str,
            arguments: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments));
            Ok(self.reply.clone())
        }
    }

    /// Connects to hubs whose URL host contains `ok`; refuses the rest.
    pub struct FakeConnector {
        pub tools: Vec<McpToolDef>,
        pub reply: Value,
    }

    #[async_trait]
    impl HubConnector for FakeConnector {
        async fn connect(&self, hub: &HubEntry) -> Result<Arc<dyn HubSession>> {
            if hub.url.contains("ok") {
                Ok(Arc::new(FakeSession {
                    tools: self.tools.clone(),
                    calls: Mutex::new(Vec::new()),
                    reply: self.reply.clone(),
                }))
            } else {
                Err(eyre::eyre!("connection refused"))
            }
        }
    }

    pub fn tool_def(name: &str) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: format!("remote {name}"),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "anyOf": [{ "required": ["q"] }]
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::hubs::test_support::MemoryStore;

    fn hub(name: &str, host: &str) -> HubEntry {
        HubEntry {
            name: name.into(),
            url: format!("https://{host}.example.com"),
            token: "wfr_x".into(),
        }
    }

    fn remote(store: MemoryStore) -> RemoteHubs {
        RemoteHubs::new(
            store,
            Arc::new(FakeConnector {
                tools: vec![tool_def("vault_set"), tool_def("workflow")],
                reply: serde_json::json!({
                    "content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}],
                    "isError": false
                }),
            }),
        )
    }

    #[test]
    fn remote_tools_carry_the_prefix_and_sanitised_schema() {
        let tool = rmcp_tool_for(&tool_def("vault_set"));
        assert_eq!(tool.name, "wf.vault_set");
        assert_eq!(tool.description.as_deref(), Some("remote vault_set"));
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        assert!(schema.get("anyOf").is_none());
        assert_eq!(schema["type"], "object");
        assert_eq!(remote_tool_name("agent_chat"), "wf.agent_chat");
    }

    #[test]
    fn call_result_text_flattens_and_maps_errors() {
        let ok =
            serde_json::json!({"content": [{"type":"text","text":"a"},{"type":"text","text":"b"}]});
        assert_eq!(call_result_text(ok).unwrap(), "a\nb");
        let err = serde_json::json!({"content": [{"type":"text","text":"boom"}], "isError": true});
        assert_eq!(call_result_text(err).unwrap_err(), "boom");
        let empty_err = serde_json::json!({"isError": true});
        assert_eq!(
            call_result_text(empty_err).unwrap_err(),
            "remote tool failed"
        );
    }

    #[tokio::test]
    async fn startup_without_an_active_hub_advertises_nothing() {
        let hubs = remote(MemoryStore::default());
        hubs.connect_active().await;
        assert!(hubs.tools().is_empty());
        let status = hubs.status();
        assert!(status.hubs.is_empty());
        assert!(status.active.is_none());
        let err = hubs
            .call("wf.workflow", JsonObject::new(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.contains("no active workforce hub"));
    }

    #[tokio::test]
    async fn switching_hubs_rebuilds_the_tool_set_and_forwards_calls() {
        let store = MemoryStore::default();
        hubs::save_hub(&store, &hub("down", "down")).unwrap();
        hubs::save_hub(&store, &hub("up", "ok")).unwrap();
        let hubs = remote(store);

        hubs.connect_active().await;
        assert!(hubs.tools().is_empty());
        assert!(matches!(
            hubs.status().connection,
            Some(ActiveState::Disconnected { .. })
        ));
        let err = hubs
            .call("wf.workflow", JsonObject::new(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.contains("unreachable"));

        assert_eq!(
            hubs.switch("up").await.unwrap(),
            ActiveState::Connected { tool_count: 2 }
        );
        let names: Vec<_> = hubs.tools().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, vec!["wf.vault_set", "wf.workflow"]);
        assert!(hubs.tool("wf.workflow").is_some());
        assert!(hubs.tool("wf.nope").is_none());
        let status = hubs.status();
        assert_eq!(status.active.as_deref(), Some("up"));
        assert!(status.hubs.iter().any(|h| h.name == "up" && h.active));
        assert!(matches!(
            status.connection,
            Some(ActiveState::Connected { tool_count: 2 })
        ));

        let mut args = JsonObject::new();
        args.insert("q".into(), serde_json::json!("x"));
        let text = hubs
            .call("wf.workflow", args, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(text, "hello\nworld");

        assert!(hubs.switch("missing").await.is_err());
        assert_eq!(hubs.status().active.as_deref(), Some("up"));
        assert!(matches!(
            hubs.switch("down").await.unwrap(),
            ActiveState::Disconnected { .. }
        ));
        assert_eq!(hubs.status().active.as_deref(), Some("down"));
        assert!(hubs.tools().is_empty());
    }

    #[tokio::test]
    async fn the_startup_connection_runs_in_the_background_and_is_awaitable() {
        let store = MemoryStore::default();
        hubs::save_hub(&store, &hub("up", "ok")).unwrap();
        let hubs = Arc::new(remote(store));
        hubs.spawn_connect_active();
        hubs.wait_ready(Duration::from_secs(5)).await;
        assert!(*hubs.settled.borrow());
        assert_eq!(hubs.tools().len(), 2);
    }

    struct HangingConnector;

    #[async_trait]
    impl HubConnector for HangingConnector {
        async fn connect(&self, _hub: &HubEntry) -> Result<Arc<dyn HubSession>> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_hub_is_reported_as_disconnected_after_the_connect_timeout() {
        let store = MemoryStore::default();
        hubs::save_hub(&store, &hub("slow", "ok")).unwrap();
        let hubs = RemoteHubs::new(store, Arc::new(HangingConnector));
        hubs.connect_active().await;
        match hubs.status().connection {
            Some(ActiveState::Disconnected { error }) => assert!(error.contains("timed out")),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(*hubs.settled.borrow());
    }

    #[tokio::test]
    async fn non_prefixed_names_are_rejected() {
        let hubs = remote(MemoryStore::default());
        let err = hubs
            .call("jira.search", JsonObject::new(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.contains("not a remote workforce tool"));
    }
}
