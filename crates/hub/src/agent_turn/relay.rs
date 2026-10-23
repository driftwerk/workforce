//! A turn's injected messages, carried to whichever route is running it.

use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use workforce_agent::runner::InjectedMessage;

/// How many messages may wait for a route to take them.
const ROUTE_BUFFER: usize = 64;

/// Forwards a turn's injected messages to its current route. A route that
/// does not serve may already have taken a person's follow-up, so every one
/// forwarded is given again to the route after it.
pub(super) struct Relay {
    state: Arc<Mutex<Routing>>,
    task: JoinHandle<()>,
}

#[derive(Default)]
struct Routing {
    current: Option<mpsc::Sender<InjectedMessage>>,
    follow_ups: Vec<InjectedMessage>,
}

impl Relay {
    pub(super) fn start(mut inject: mpsc::Receiver<InjectedMessage>) -> Self {
        let state = Arc::new(Mutex::new(Routing::default()));
        let routing = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Some(message) = inject.recv().await {
                let mut routing = routing.lock().await;
                if matches!(message, InjectedMessage::UserMessage { .. }) {
                    routing.follow_ups.push(message.clone());
                }
                if let Some(route) = &routing.current {
                    let _ = route.try_send(message);
                }
            }
        });
        Self { state, task }
    }

    /// The messages the next route takes: every follow-up so far, then what
    /// arrives while it runs.
    pub(super) async fn next_route(&self) -> mpsc::Receiver<InjectedMessage> {
        let mut routing = self.state.lock().await;
        let (tx, rx) = mpsc::channel(ROUTE_BUFFER + routing.follow_ups.len());
        routing
            .follow_ups
            .iter()
            .cloned()
            .for_each(|message| drop(tx.try_send(message)));
        routing.current = Some(tx);
        rx
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn follow_up(text: &str) -> InjectedMessage {
        InjectedMessage::UserMessage {
            text: text.into(),
            user: "alice".into(),
            external_id: None,
        }
    }

    /// The next `n` messages `rx` is given, as text, then that no more is
    /// waiting.
    async fn texts(rx: &mut mpsc::Receiver<InjectedMessage>, n: usize) -> Vec<String> {
        let mut texts = Vec::new();
        for _ in 0..n {
            let message = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("forwarded in time")
                .expect("the relay is running");
            texts.push(match message {
                InjectedMessage::UserMessage { text, .. } => text,
                other => format!("{other:?}"),
            });
        }
        assert!(rx.try_recv().is_err(), "nothing more: {texts:?}");
        texts
    }

    #[tokio::test]
    async fn forwarded_user_messages_replay_after_hop() {
        let (tx, rx) = mpsc::channel(8);
        let relay = Relay::start(rx);
        let mut first = relay.next_route().await;
        tx.send(follow_up("also check staging")).await.unwrap();
        tx.send(InjectedMessage::ApprovalResponse {
            request_id: "r1".into(),
            approved: true,
            user: "alice".into(),
            choice: None,
        })
        .await
        .unwrap();
        assert_eq!(
            texts(&mut first, 2).await.len(),
            2,
            "the running route takes both"
        );

        let mut second = relay.next_route().await;
        tx.send(follow_up("and prod")).await.unwrap();
        assert_eq!(
            texts(&mut second, 2).await,
            ["also check staging", "and prod"],
            "the next route is given the follow-up the first took, and no stale approval"
        );
    }
}
