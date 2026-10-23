//! Pushing an answer saved to a session to whoever is watching it live. A
//! resumed turn runs on whichever replica resumed it, while the page or app
//! waiting on the session may be connected to any replica, so the push goes
//! over NATS to every replica's gateway broadcast.

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::warn;

use crate::broadcast::BroadcastMessage;

const APPENDED_SUBJECT: &str = "workforce.sessions.appended";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Appended {
    session_id: String,
    text: String,
}

impl Appended {
    fn message(self) -> BroadcastMessage {
        BroadcastMessage::new(self.session_id, "message", self.text)
    }
}

/// Where answers saved to a session are pushed.
#[derive(Clone)]
pub struct SessionPush {
    nats: Option<async_nats::Client>,
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
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
        let appended = Appended {
            session_id: session_id.to_string(),
            text: text.to_string(),
        };
        let Some(nats) = &self.nats else {
            let _ = self.broadcast_tx.send(appended.message());
            return;
        };
        let payload = serde_json::to_vec(&appended).expect("two strings serialise");
        if let Err(e) = nats.publish(APPENDED_SUBJECT, payload.into()).await {
            warn!(session_id, error = %e, "pushing a resumed answer; sending it locally");
            let _ = self.broadcast_tx.send(appended.message());
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
                        match serde_json::from_slice::<Appended>(&push.payload) {
                            Ok(appended) => {
                                let _ = broadcast_tx.send(appended.message());
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

    #[tokio::test]
    async fn without_nats_an_answer_goes_to_this_replica_s_connections() {
        let (tx, mut rx) = broadcast::channel(8);
        SessionPush::new(None, tx).deliver("s1", "the answer").await;
        let pushed = rx.recv().await.unwrap();
        assert_eq!(
            (
                pushed.session_id.as_str(),
                pushed.msg_type.as_str(),
                pushed.content.as_str()
            ),
            ("s1", "message", "the answer")
        );
    }

    #[test]
    fn a_push_carries_its_session_and_text_between_replicas() {
        let appended = Appended {
            session_id: "s1".into(),
            text: "the answer".into(),
        };
        let wire = serde_json::to_vec(&appended).unwrap();
        let relayed: Appended = serde_json::from_slice(&wire).unwrap();
        assert_eq!(relayed, appended);
        assert_eq!(relayed.message().session_id, "s1");
    }
}
