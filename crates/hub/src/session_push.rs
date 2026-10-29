//! Pushing what lands in a session outside a live turn to whoever is watching
//! it: an answer a resumed turn saved, or a delivery to a Desk conversation,
//! with its signal to the person's Desk hub links. The turn or delivery runs
//! on whichever replica ran it, while the page or app waiting on it may be
//! connected to any replica, so the push goes over NATS to every replica's
//! gateway broadcast.

use std::sync::OnceLock;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::warn;
use workforce_common::channels::desk::DeskDelivered;

use crate::broadcast::BroadcastMessage;

const APPENDED_SUBJECT: &str = "workforce.sessions.appended";

/// What one replica tells the others, on one subject. Untagged, so a
/// replica on the previous build still reads an append it sends.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
enum Relayed {
    Appended {
        session_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    Delivered {
        owner: String,
        delivered: DeskDelivered,
    },
}

impl Relayed {
    fn message(self) -> BroadcastMessage {
        match self {
            Self::Appended {
                session_id,
                text,
                id,
            } => BroadcastMessage {
                id,
                ..BroadcastMessage::new(session_id, "message", text)
            },
            Self::Delivered { owner, delivered } => {
                BroadcastMessage::desk_delivered(&owner, delivered)
            }
        }
    }
}

/// Where what lands in a session outside a turn is pushed.
#[derive(Clone)]
pub struct SessionPush {
    nats: Option<async_nats::Client>,
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
}

static INSTALLED: OnceLock<SessionPush> = OnceLock::new();

/// Make `push` the hub's, for the paths that run outside any connection:
/// deliveries from jobs, workflows and heartbeat checks.
pub fn install(push: SessionPush) {
    if INSTALLED.set(push).is_err() {
        warn!("session push already installed; keeping the first");
    }
}

/// The hub's push, once [`install`]ed.
pub fn installed() -> Option<&'static SessionPush> {
    INSTALLED.get()
}

/// A receiver of what the test process's push sends, installing that push,
/// on this process only, the first time. Tests share it, so each looks for
/// its own sessions' frames.
#[cfg(test)]
pub(crate) fn subscribe_for_tests() -> broadcast::Receiver<BroadcastMessage> {
    static SENDER: OnceLock<broadcast::Sender<BroadcastMessage>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (tx, _) = broadcast::channel(1024);
        install(SessionPush::new(None, tx.clone()));
        tx
    });
    sender.subscribe()
}

impl SessionPush {
    pub fn new(
        nats: Option<async_nats::Client>,
        broadcast_tx: broadcast::Sender<BroadcastMessage>,
    ) -> Self {
        Self { nats, broadcast_tx }
    }

    /// Push `text` to every connection watching `session_id`. Best effort:
    /// the answer is in the session already, and a client that misses the
    /// push reads it from there.
    pub async fn deliver(&self, session_id: &str, text: &str) {
        self.message(session_id, text, None).await;
    }

    /// [`Self::deliver`] for a message saved under `id`, which the pushed
    /// frame carries.
    pub async fn message(&self, session_id: &str, text: &str, id: Option<&str>) {
        self.relay(Relayed::Appended {
            session_id: session_id.to_string(),
            text: text.to_string(),
            id: id.map(str::to_string),
        })
        .await;
    }

    /// Tell every Desk hub link of `owner`'s, on every replica, that
    /// `delivered` landed in one of their conversations.
    pub async fn delivered(&self, owner: &str, delivered: DeskDelivered) {
        self.relay(Relayed::Delivered {
            owner: owner.to_string(),
            delivered,
        })
        .await;
    }

    async fn relay(&self, relayed: Relayed) {
        let Some(nats) = &self.nats else {
            let _ = self.broadcast_tx.send(relayed.message());
            return;
        };
        let payload = match serde_json::to_vec(&relayed) {
            Ok(payload) => payload,
            Err(e) => {
                warn!(error = %e, "encoding a session push; sending it locally");
                let _ = self.broadcast_tx.send(relayed.message());
                return;
            }
        };
        if let Err(e) = nats.publish(APPENDED_SUBJECT, payload.into()).await {
            warn!(error = %e, "relaying a session push; sending it locally");
            let _ = self.broadcast_tx.send(relayed.message());
        }
    }
}

const RESUBSCRIBE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Relay every replica's pushes into this replica's gateway broadcast, for
/// as long as the hub runs.
pub fn spawn_relay(
    nats: async_nats::Client,
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match nats.subscribe(APPENDED_SUBJECT).await {
                Ok(mut pushes) => {
                    while let Some(push) = pushes.next().await {
                        match serde_json::from_slice::<Relayed>(&push.payload) {
                            Ok(relayed) => {
                                let _ = broadcast_tx.send(relayed.message());
                            }
                            Err(e) => warn!(error = %e, "an unreadable session push"),
                        }
                    }
                    warn!("session pushes ended; subscribing again");
                }
                Err(e) => warn!(error = %e, "subscribing to session pushes"),
            }
            tokio::time::sleep(RESUBSCRIBE_AFTER).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::channels::desk::{DeliveryKind, DeskPlace};

    fn delivered() -> DeskDelivered {
        DeskDelivered {
            agent_id: "scout".into(),
            channel: "desk".into(),
            session_id: "s1".into(),
            message_id: "m1".into(),
            kind: DeliveryKind::Reminder,
            place: DeskPlace::Dm,
            spoken: false,
        }
    }

    #[tokio::test]
    async fn without_nats_an_answer_goes_to_this_replica_s_connections() {
        let (tx, mut rx) = broadcast::channel(8);
        SessionPush::new(None, tx).deliver("s1", "the answer").await;
        let pushed = rx.recv().await.unwrap();
        assert_eq!(
            (
                pushed.session_id.as_str(),
                pushed.msg_type.as_str(),
                pushed.content.as_str(),
                pushed.id
            ),
            ("s1", "message", "the answer", None)
        );
    }

    #[tokio::test]
    async fn a_delivery_is_a_message_with_its_id_and_a_signal_to_its_owner() {
        let (tx, mut rx) = broadcast::channel(8);
        let push = SessionPush::new(None, tx);
        push.message("s1", "a reminder", Some("m1")).await;
        push.delivered("person-1", delivered()).await;
        let message = rx.recv().await.unwrap();
        assert_eq!(
            (message.msg_type.as_str(), message.id.as_deref()),
            ("message", Some("m1"))
        );
        let signal = rx.recv().await.unwrap();
        assert_eq!(signal.msg_type, crate::broadcast::DESK_DELIVERED);
        assert_eq!(signal.target_identity.as_deref(), Some("person-1"));
        assert_eq!(signal.delivered, Some(delivered()));
        assert!(signal.session_id.is_empty());
    }

    #[test]
    fn pushes_read_the_same_on_every_replica_old_and_new() {
        let appended = Relayed::Appended {
            session_id: "s1".into(),
            text: "the answer".into(),
            id: None,
        };
        let wire = serde_json::to_value(&appended).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"session_id": "s1", "text": "the answer"}),
            "an append is what a replica on the previous build sends and reads"
        );
        assert_eq!(serde_json::from_value::<Relayed>(wire).unwrap(), appended);
        let signal = Relayed::Delivered {
            owner: "person-1".into(),
            delivered: delivered(),
        };
        let relayed: Relayed =
            serde_json::from_slice(&serde_json::to_vec(&signal).unwrap()).unwrap();
        assert_eq!(relayed, signal);
        assert_eq!(
            relayed.message().target_identity.as_deref(),
            Some("person-1")
        );
    }

    /// A delivery made on one replica reaches the hub links on another,
    /// against a real NATS server: `WORKFORCE_TEST_NATS_URL=nats://localhost:4222`.
    #[tokio::test]
    async fn a_signal_crosses_to_every_replica() {
        let Ok(url) = std::env::var("WORKFORCE_TEST_NATS_URL") else {
            return;
        };
        let (here, _) = broadcast::channel(16);
        let (there, mut heard) = broadcast::channel(16);
        let _relay = spawn_relay(async_nats::connect(&url).await.unwrap(), there);
        let push = SessionPush::new(Some(async_nats::connect(&url).await.unwrap()), here);
        let owner = uuid::Uuid::new_v4().to_string();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                push.delivered(&owner, delivered()).await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                while let Ok(frame) = heard.try_recv() {
                    if frame.target_identity.as_deref() == Some(owner.as_str()) {
                        return frame;
                    }
                }
            }
        })
        .await
        .expect("the other replica hears it");
        assert_eq!(received.delivered, Some(delivered()));
    }
}
