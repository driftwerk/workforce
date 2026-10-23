//! Workforce Gateway Client
//!
//! A client library for connecting to the Workforce gateway via WebSocket or REST.
//!
//! # Usage
//!
//! ## WebSocket (streaming, full-featured)
//!
//! ```no_run
//! use workforce_gateway_client::{GatewayClient, GatewayClientConfig, GatewayEvent};
//!
//! # async fn example() -> eyre::Result<()> {
//! let config = GatewayClientConfig {
//!     url: "ws://localhost:8091".to_string(),
//!     token: "your-token".to_string(),
//!     ..Default::default()
//! };
//!
//! let mut client = GatewayClient::new(config);
//! let mut events = client.take_events().unwrap();
//! client.connect().await?;
//!
//! // Send a message
//! client.send_chat("Hello!").await?;
//!
//! // Receive events
//! while let Some(event) = events.recv().await {
//!     match event {
//!         GatewayEvent::Message(msg) => println!("{}", msg.text().unwrap_or("")),
//!         GatewayEvent::Disconnected { reason } => break,
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## REST (simple, blocking)
//!
//! ```no_run
//! use workforce_gateway_client::GatewayRestClient;
//!
//! # async fn example() -> eyre::Result<()> {
//! let client = GatewayRestClient::new("http://localhost:8091", "your-token");
//! let response = client.chat("Hello!").await?;
//! println!("{}", response.response);
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod protocol;

pub use client::{
    gateway_available, try_connect, AgentVaultCopyOutcome, AgentVaultCopyResponse, GatewayClient,
    GatewayClientConfig, GatewayEvent, GatewayRestClient,
};
pub use protocol::{
    AgentDetailResponse, AgentInfo, AgentStatsResponse, AgentToolInfo, AgentsResponse, ChatOutcome,
    ChatRequest, ChatResponse, ClientMessage, CreateAgentRequest, NodeInvokeRequest,
    NodeInvokeResult, ServerMessage, ServerMessageKind, SessionInfo, SessionsResponse,
    UpdateAgentRequest,
};
