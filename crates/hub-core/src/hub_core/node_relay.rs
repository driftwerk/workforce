//! Reaching a person's node from whichever hub instance runs their turn.
//!
//! [`NodePresence`] says which nodes are online and whose they are, so a
//! turn on any instance sees its person's nodes. [`NodeRelay`] carries calls
//! to the one instance holding a node's connection, over NATS request/reply
//! to [`subjects::node`]: only that instance subscribes, so no responders
//! means the node is offline. The holder then publishes the call's
//! [`NodeEvent`]s to [`subjects::node_reply`], which the caller subscribed
//! to before it asked. Any instance can take that publishing up: a node
//! that reconnects elsewhere keeps reporting to the turn waiting on it.

use std::sync::Weak;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use workforce_core::computer::{DeskNode, OnlineDesk};
use workforce_gateway_client::protocol::NodeToolSpec;
use workforce_nats::subjects;

use super::local_agent_registry::{
    CallError, CallSink, LocalAgentRegistry, NodeCall, NodeEvent, CALL_EVENTS,
};

/// How long the holder has to take a call before the node counts as
/// offline.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Which nodes are online, and whose: kept where every instance reads it.
#[async_trait]
pub trait NodePresence: Send + Sync {
    /// Records that this instance holds `key`'s connection, for `identity`.
    async fn put(&self, key: &str, identity: &str, kind: &PresenceKind) -> Result<()>;
    /// Marks the key offline, keeping when it was last seen.
    async fn gone(&self, key: &str) -> Result<()>;
    /// Forgets the key: its device was unpaired.
    async fn forget(&self, key: &str) -> Result<()>;
    async fn desks_of(&self, identity: &str) -> Result<Desks>;
    /// The `Agent` nodes online: `identity`'s, or everyone's.
    async fn agents(&self, identity: Option<&str>) -> Result<Vec<PresentAgent>>;
}

/// A node as presence keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresenceKind {
    Agent(AgentPresence),
    Desk { device_id: String, node: DeskNode },
}

/// What a turn on any instance needs of a local agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPresence {
    pub name: String,
    pub capabilities: Vec<String>,
    /// The tools it offers, as admitted when it connected.
    pub tools: Vec<NodeToolSpec>,
    pub connected_at: DateTime<Utc>,
}

/// A local agent online, and whose it is.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentAgent {
    pub key: String,
    pub identity_id: String,
    pub agent: AgentPresence,
}

/// A person's Desks as a turn sees them: those online, and when one was
/// last seen.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Desks {
    pub online: Vec<OnlineDesk>,
    /// The latest a Desk of theirs that is offline now was seen.
    pub last_seen: Option<DateTime<Utc>>,
    /// The device ids of every Desk with a row, online or not: it tells
    /// "never used" apart from "offline".
    pub known: Vec<String>,
}

/// What a caller asks of the instance holding a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeControl {
    Call { caller: String, call: NodeCall },
    Cancel { request_id: String },
    Collect { caller: String, request_id: String },
}

/// The holder's answer to a [`NodeControl`]; the call's events follow on
/// its reply subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum ControlReply {
    Accepted { accepted: bool },
    Refused { refused: String },
}

impl ControlReply {
    fn of(handled: Result<(), CallError>) -> Self {
        match handled {
            Ok(()) => Self::Accepted { accepted: true },
            Err(e) => Self::Refused {
                refused: e.to_string(),
            },
        }
    }
}

/// Whether `id` can name a call: a UUID in its hyphenated, lowercase form.
/// It becomes part of a subject, so nothing else may.
pub fn valid_request_id(id: &str) -> bool {
    uuid::Uuid::try_parse(id).is_ok_and(|uuid| uuid.to_string() == id)
}

/// Calls to nodes across instances, over NATS.
#[derive(Clone)]
pub struct NodeRelay {
    nats: async_nats::Client,
}

impl NodeRelay {
    pub fn new(nats: async_nats::Client) -> Self {
        Self { nats }
    }

    /// Answer calls to node `key`'s `connection` on this instance, until the
    /// returned task is aborted.
    pub(super) async fn listen(
        &self,
        registry: Weak<LocalAgentRegistry>,
        key: &str,
        connection: u64,
    ) -> Result<JoinHandle<()>> {
        let sub = self.nats.subscribe(subjects::node(key)).await?;
        self.nats.flush().await?;
        let (relay, key) = (self.clone(), key.to_string());
        Ok(tokio::spawn(relay.pump(registry, sub, key, connection)))
    }

    async fn pump(
        self,
        registry: Weak<LocalAgentRegistry>,
        mut sub: async_nats::Subscriber,
        key: String,
        connection: u64,
    ) {
        while let Some(msg) = sub.next().await {
            let Some(reply_to) = msg.reply else { continue };
            let control = match serde_json::from_slice::<NodeControl>(&msg.payload) {
                Ok(control) => control,
                Err(e) => {
                    warn!(node = %key, error = %e, "Undecodable node control");
                    continue;
                }
            };
            let Some(registry) = registry.upgrade() else {
                return;
            };
            let (relay, key) = (self.clone(), key.clone());
            tokio::spawn(async move {
                // A connection superseded on this instance leaves the
                // answer to its successor's listener.
                if !registry.holds(&key, connection).await {
                    return;
                }
                let handled = relay.handle(&registry, &key, control).await;
                match serde_json::to_vec(&ControlReply::of(handled)) {
                    Ok(payload) => {
                        if let Err(e) = relay.nats.publish(reply_to, payload.into()).await {
                            warn!(node = %key, error = %e, "Replying to node control failed");
                        }
                    }
                    Err(e) => warn!(error = %e, "Encoding node control reply failed"),
                }
            });
        }
    }

    async fn handle(
        &self,
        registry: &LocalAgentRegistry,
        key: &str,
        control: NodeControl,
    ) -> Result<(), CallError> {
        match control {
            NodeControl::Call { caller, call } => {
                let sink = self.sink(checked(call.request_id())?)?;
                registry.accept(key, &caller, call, sink).await
            }
            NodeControl::Cancel { request_id } => {
                registry.withdraw(key, &request_id).await;
                Ok(())
            }
            NodeControl::Collect { caller, request_id } => {
                let sink = self.sink(checked(&request_id)?)?;
                registry.reattach(key, &request_id, &caller, sink).await
            }
        }
    }

    /// A sink publishing one call's events to its reply subject, in order.
    pub(super) fn sink(&self, request_id: &str) -> Result<CallSink, CallError> {
        let subject = subjects::node_reply(checked(request_id)?);
        let (tx, mut rx) = mpsc::channel::<NodeEvent>(CALL_EVENTS);
        let nats = self.nats.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let payload = match serde_json::to_vec(&event) {
                    Ok(payload) => payload,
                    Err(e) => {
                        warn!(error = %e, "Encoding a node event failed");
                        continue;
                    }
                };
                if let Err(e) = nats.publish(subject.clone(), payload.into()).await {
                    warn!(%subject, error = %e, "Publishing a node event failed");
                }
            }
        });
        Ok(tx)
    }

    pub(super) async fn call(
        &self,
        key: &str,
        caller: &str,
        call: NodeCall,
    ) -> Result<mpsc::Receiver<NodeEvent>, CallError> {
        let request_id = call.request_id().to_string();
        let control = NodeControl::Call {
            caller: caller.to_string(),
            call,
        };
        self.ask(key, &request_id, &control).await
    }

    pub(super) async fn collect(
        &self,
        key: &str,
        request_id: &str,
        caller: &str,
    ) -> Result<mpsc::Receiver<NodeEvent>, CallError> {
        let control = NodeControl::Collect {
            caller: caller.to_string(),
            request_id: request_id.to_string(),
        };
        self.ask(key, request_id, &control).await
    }

    pub(super) async fn cancel(&self, key: &str, request_id: &str) {
        let control = NodeControl::Cancel {
            request_id: request_id.to_string(),
        };
        if let Err(e) = self.request(key, &control).await {
            debug!(node = %key, request_id, error = %e, "Cancelling a node call reached no one");
        }
    }

    /// Send `control` for `request_id`'s call, returning its events once
    /// the holder took it. The reply subject is listened on first, so no
    /// event is published before anyone hears it.
    async fn ask(
        &self,
        key: &str,
        request_id: &str,
        control: &NodeControl,
    ) -> Result<mpsc::Receiver<NodeEvent>, CallError> {
        let subject = subjects::node_reply(checked(request_id)?);
        let mut events = self.nats.subscribe(subject).await.map_err(relay_error)?;
        self.nats.flush().await.map_err(relay_error)?;
        self.request(key, control).await?;
        let (tx, rx) = mpsc::channel(CALL_EVENTS);
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    () = tx.closed() => return,
                    msg = events.next() => match msg {
                        Some(msg) => msg,
                        None => return,
                    },
                };
                let event = match serde_json::from_slice::<NodeEvent>(&msg.payload) {
                    Ok(event) => event,
                    Err(e) => {
                        warn!(error = %e, "Undecodable node event");
                        continue;
                    }
                };
                let last = event.is_last();
                if tx.send(event).await.is_err() || last {
                    return;
                }
            }
        });
        Ok(rx)
    }

    async fn request(&self, key: &str, control: &NodeControl) -> Result<(), CallError> {
        let payload = serde_json::to_vec(control).map_err(relay_error)?;
        let reply = tokio::time::timeout(
            ACCEPT_TIMEOUT,
            self.nats.request(subjects::node(key), payload.into()),
        )
        .await
        .map_err(|_| CallError::Offline(key.to_string()))?
        .map_err(|e| match e.kind() {
            async_nats::RequestErrorKind::NoResponders | async_nats::RequestErrorKind::TimedOut => {
                CallError::Offline(key.to_string())
            }
            _ => relay_error(e),
        })?;
        match serde_json::from_slice::<ControlReply>(&reply.payload) {
            Ok(ControlReply::Accepted { .. }) => Ok(()),
            Ok(ControlReply::Refused { refused }) => Err(CallError::Refused(refused)),
            Err(e) => Err(relay_error(e)),
        }
    }
}

fn checked(request_id: &str) -> Result<&str, CallError> {
    valid_request_id(request_id)
        .then_some(request_id)
        .ok_or_else(|| CallError::Refused(format!("'{request_id}' is not a call id")))
}

fn relay_error(e: impl Into<eyre::Report>) -> CallError {
    CallError::Relay(e.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_uuid_names_a_call() {
        let id = uuid::Uuid::new_v4().to_string();
        assert!(valid_request_id(&id));
        for bad in [
            id.to_uppercase(),
            format!("{{{id}}}"),
            id.replace('-', ""),
            "r1".into(),
            "a.b".into(),
            "*".into(),
            ">".into(),
        ] {
            assert!(!valid_request_id(&bad), "{bad}");
        }
    }

    #[test]
    fn the_holders_answer_reads_as_its_wire_form() {
        let accepted = serde_json::to_value(ControlReply::of(Ok(()))).unwrap();
        assert_eq!(accepted, serde_json::json!({"accepted": true}));
        let refused =
            serde_json::to_value(ControlReply::of(Err(CallError::Refused("no".into())))).unwrap();
        assert_eq!(refused, serde_json::json!({"refused": "no"}));
        assert_eq!(
            serde_json::from_value::<ControlReply>(refused).unwrap(),
            ControlReply::Refused {
                refused: "no".into()
            }
        );
    }
}
