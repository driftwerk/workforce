//! The MCP bridge a job talks to: an HTTP endpoint on the host, reachable
//! from the scope, that forwards each request to the hub and returns the
//! reply. A plain job's bridge forwards over the job's own connection and goes
//! with it. A durable job's forwards to whichever client is attached to the
//! job at the time, and keeps its address across restarts of the daemon.

use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use eyre::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use workforce_agent::tools::exec_runner::{CodexMcpServer, JobLink, ATTACH_WAIT};

/// The name a scope reaches the host by, as the container runtime maps it.
const HOST_ALIAS: &str = "host.docker.internal";

/// Ports durable bridges are served on. The host reserves them from the
/// ephemeral range, so an address a job was given is still free for it after
/// a restart.
pub const DURABLE_BRIDGE_PORTS: RangeInclusive<u16> = 41000..=41999;

const REBIND_ATTEMPTS: u32 = 20;
const REBIND_PAUSE: Duration = Duration::from_millis(250);

/// Where a bridge listens and the bearer it expects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeAddress {
    pub port: u16,
    pub token: String,
}

/// Where a bridge sends a tool call.
#[async_trait]
pub trait McpRoute: Send + Sync {
    async fn call(&self, body: String) -> Result<String>;
}

#[async_trait]
impl McpRoute for JobLink {
    async fn call(&self, body: String) -> Result<String> {
        self.mcp_call(body).await
    }
}

/// A durable job's client of the moment, if one is attached.
pub type Attachment = watch::Receiver<Option<Arc<JobLink>>>;

#[async_trait]
impl McpRoute for Attachment {
    /// Sent once to the client attached when the call is made, waiting for one
    /// if none is. A reply lost with its connection is not retried: the tool
    /// may already have acted, and the job decides whether to call again.
    async fn call(&self, body: String) -> Result<String> {
        let mut attachment = self.clone();
        let link = tokio::time::timeout(ATTACH_WAIT, attachment.wait_for(Option::is_some))
            .await
            .map_err(|_| {
                eyre::eyre!(
                    "no hub attached to the job within {} seconds",
                    ATTACH_WAIT.as_secs()
                )
            })?
            .context("the job has ended")?
            .clone()
            .expect("waited for an attached client");
        link.mcp_call(body).await
    }
}

pub struct Bridge {
    address: BridgeAddress,
    /// How long a call may be held before it reaches a client.
    hold: Duration,
    task: JoinHandle<()>,
}

struct BridgeState {
    route: Arc<dyn McpRoute>,
    token: String,
}

impl Bridge {
    /// A bridge for one plain job on a port of the system's choosing.
    pub async fn start(link: Arc<JobLink>) -> Result<Self> {
        let listener = TcpListener::bind("0.0.0.0:0")
            .await
            .context("binding the MCP bridge")?;
        Ok(Self::serve(link, listener, new_token(), Duration::ZERO))
    }

    /// A bridge for a durable job: at `address` when it had one before a
    /// restart, otherwise on the first free port of [`DURABLE_BRIDGE_PORTS`]
    /// after `taken`.
    pub async fn durable(
        route: Attachment,
        address: Option<BridgeAddress>,
        taken: &[u16],
    ) -> Result<Self> {
        let route: Arc<dyn McpRoute> = Arc::new(route);
        if let Some(address) = address {
            let listener = rebind(address.port).await?;
            return Ok(Self::serve(route, listener, address.token, ATTACH_WAIT));
        }
        for port in DURABLE_BRIDGE_PORTS.filter(|port| !taken.contains(port)) {
            if let Ok(listener) = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
                return Ok(Self::serve(route, listener, new_token(), ATTACH_WAIT));
            }
        }
        bail!("every port of the durable MCP bridge range is in use")
    }

    fn serve(
        route: Arc<dyn McpRoute>,
        listener: TcpListener,
        token: String,
        hold: Duration,
    ) -> Self {
        let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
        let state = Arc::new(BridgeState {
            route,
            token: token.clone(),
        });
        let app = Router::new().route("/mcp", post(handle)).with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            address: BridgeAddress { port, token },
            hold,
            task,
        }
    }

    pub fn address(&self) -> &BridgeAddress {
        &self.address
    }

    pub fn port(&self) -> u16 {
        self.address.port
    }

    /// The `[mcp_servers.<name>]` block Codex reads inside the scope. Codex
    /// waits for a call as long as the tool may take plus however long this
    /// bridge may hold the call for a client, so it never gives up first.
    pub fn codex_config(&self, server_name: &str, tool_timeout_secs: u64) -> String {
        CodexMcpServer::http(
            &self.url(),
            &self.address.token,
            tool_timeout_secs + self.hold.as_secs(),
        )
        .toml_block(server_name)
    }

    fn url(&self) -> String {
        format!("http://{HOST_ALIAS}:{}/mcp", self.port())
    }

    /// The MCP config Claude Code reads inside the scope.
    pub fn config(&self, server_name: &str) -> String {
        serde_json::json!({
            "mcpServers": {
                server_name: {
                    "type": "http",
                    "url": self.url(),
                    "headers": { "Authorization": format!("Bearer {}", self.address.token) },
                }
            }
        })
        .to_string()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The port a durable bridge had, bound again. The listener it had closes
/// only once its task is torn down, so a busy port is retried for a while.
async fn rebind(port: u16) -> Result<TcpListener> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let mut last = None;
    for _ in 0..REBIND_ATTEMPTS {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last = Some(e);
                tokio::time::sleep(REBIND_PAUSE).await;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("binding the MCP bridge again on port {port}"))
            }
        }
    }
    Err(last.expect("at least one attempt"))
        .with_context(|| format!("binding the MCP bridge again on port {port}"))
}

fn new_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn handle(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !bearer_matches(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.route.call(body).await {
        Ok(reply) if reply.is_empty() => StatusCode::ACCEPTED.into_response(),
        Ok(reply) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            reply,
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

fn bearer_matches(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use workforce_agent::tools::exec_runner::ServerFrame;

    async fn bridge_with_echo() -> (Bridge, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel(8);
        let link = Arc::new(JobLink::for_tests(tx));
        let bridge_link = Arc::clone(&link);
        let answering = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let ServerFrame::McpRequest { id, body } = frame {
                    link.fulfil_for_tests(
                        id,
                        if body == "notify" {
                            String::new()
                        } else {
                            format!("echo:{body}")
                        },
                    );
                }
            }
        });
        (Bridge::start(bridge_link).await.unwrap(), answering)
    }

    #[tokio::test]
    async fn codex_waits_as_long_as_a_bridge_may_hold_its_call() {
        let (plain, _answering) = bridge_with_echo().await;
        let (_attached, attachment) = watch::channel(None);
        let durable = Bridge::durable(attachment, None, &[]).await.unwrap();
        let timeout = |bridge: &Bridge| {
            let block: toml::Value = bridge.codex_config("workforce", 300).parse().unwrap();
            block["mcp_servers"]["workforce"]["tool_timeout_sec"]
                .as_integer()
                .unwrap()
        };
        assert_eq!(timeout(&plain), 300);
        assert_eq!(timeout(&durable), 300 + ATTACH_WAIT.as_secs() as i64);
    }

    #[tokio::test]
    async fn a_request_with_the_token_is_forwarded_and_answered() {
        let (bridge, _answering) = bridge_with_echo().await;
        let client = reqwest::Client::new();
        let reply = client
            .post(format!("http://127.0.0.1:{}/mcp", bridge.port()))
            .bearer_auth(&bridge.address().token)
            .body("ping")
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.text().await.unwrap(), "echo:ping");

        let accepted = client
            .post(format!("http://127.0.0.1:{}/mcp", bridge.port()))
            .bearer_auth(&bridge.address().token)
            .body("notify")
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), 202);
    }

    #[tokio::test]
    async fn the_wrong_token_is_refused() {
        let (bridge, _answering) = bridge_with_echo().await;
        let reply = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/mcp", bridge.port()))
            .bearer_auth("nope")
            .body("ping")
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 401);
    }

    #[tokio::test]
    async fn a_durable_bridge_holds_a_call_until_a_client_attaches_and_keeps_its_address() {
        let (attached, attachment) = watch::channel(None);
        let bridge = Bridge::durable(attachment.clone(), None, &[])
            .await
            .unwrap();
        assert!(DURABLE_BRIDGE_PORTS.contains(&bridge.port()));
        let address = bridge.address().clone();
        let call = tokio::spawn({
            let address = address.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("http://127.0.0.1:{}/mcp", address.port))
                    .bearer_auth(&address.token)
                    .body("ping")
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (tx, mut rx) = mpsc::channel(8);
        let link = Arc::new(JobLink::for_tests(tx));
        attached.send_replace(Some(Arc::clone(&link)));
        let ServerFrame::McpRequest { id, body } = rx.recv().await.unwrap() else {
            panic!("expected the held call")
        };
        link.fulfil_for_tests(id, format!("pong:{body}"));
        assert_eq!(call.await.unwrap(), "pong:ping");

        drop(bridge);
        let again = Bridge::durable(attachment, Some(address.clone()), &[])
            .await
            .unwrap();
        assert_eq!(again.address(), &address);
    }

    #[tokio::test]
    async fn a_new_durable_bridge_skips_the_ports_other_jobs_hold() {
        let (_attached, attachment) = watch::channel(None);
        let first = Bridge::durable(attachment.clone(), None, &[])
            .await
            .unwrap();
        let second = Bridge::durable(attachment, None, &[first.port()])
            .await
            .unwrap();
        assert_ne!(first.port(), second.port());
    }

    #[tokio::test]
    async fn the_config_names_the_host_alias_and_the_token() {
        let (bridge, _answering) = bridge_with_echo().await;
        let config: serde_json::Value = serde_json::from_str(&bridge.config("workforce")).unwrap();
        let server = &config["mcpServers"]["workforce"];
        assert_eq!(server["type"], "http");
        assert_eq!(
            server["url"],
            format!("http://host.docker.internal:{}/mcp", bridge.port())
        );
        assert_eq!(
            server["headers"]["Authorization"],
            format!("Bearer {}", bridge.address().token)
        );
    }
}
