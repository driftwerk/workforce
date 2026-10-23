//! Voice announcements: pushing text to voice-bridge clients so a scheduled
//! job's result is spoken by the device the user asked from.
//!
//! A voice bridge registers itself with a `voice_listen` gateway message; the
//! connection is recorded here for as long as it lives. The scheduler asks the
//! [`VoiceAnnouncer`] to deliver; locally-registered bridges are reached over
//! the in-process broadcast bus, and in multi-instance deployments a NATS
//! request/reply hop reaches the instance holding the bridge's socket — the
//! reply is what tells the scheduler whether anything will actually speak, so
//! silence can fall back to Slack instead of losing the reminder.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use eyre::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::broadcast::BroadcastMessage;

/// How long to wait for another instance to claim an announcement before
/// concluding no bridge is connected anywhere. Announcements fire from the
/// scheduler tick, so this bounds how long a fire can stall.
const ANNOUNCE_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a connection may receive content belonging to `target_identity`.
///
/// Strict equality, fail-closed, and the target is never optional: an
/// announcement always belongs to the person whose job produced it. Every
/// WebSocket connection is a paired device or a JWT session (the static
/// operator token cannot open one), so a listener with no identity is a
/// paired-but-unlinked device — and pairing alone must not be enough to hear
/// someone's reminders. Link the device to its owner (the bridge's
/// `voice_device` pairing auto-links) and the match holds.
pub fn identity_admits(conn_identity: Option<&str>, target_identity: &str) -> bool {
    conn_identity == Some(target_identity)
}

struct Listener {
    id: u64,
    identity: Option<String>,
}

/// Whether a channel can carry an announcement for a given identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A registered bridge admits it.
    Admitted,
    /// Nothing is registered on the channel — the device is off, or the
    /// bridge's socket lives on another hub instance.
    NoListener,
    /// Bridges are registered but none admits this identity. Almost always a
    /// setup mistake: the device paired but was never linked to its owner.
    Refused { listeners: usize },
}

/// Voice-bridge connections registered on THIS hub instance, by channel name.
#[derive(Default)]
pub struct VoiceListeners {
    inner: RwLock<HashMap<String, Vec<Listener>>>,
    next_id: AtomicU64,
}

impl VoiceListeners {
    /// Record a connection listening on `channel`. Registration lives until
    /// the returned guard drops — tie it to the connection's task so a closed
    /// socket deregisters without a separate cleanup path.
    pub fn register(
        self: &Arc<Self>,
        channel: &str,
        identity: Option<String>,
    ) -> VoiceListenerGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        info!(channel, identity = ?identity, "voice bridge registered");
        self.inner
            .write()
            .expect("voice listener lock")
            .entry(channel.to_string())
            .or_default()
            .push(Listener { id, identity });
        VoiceListenerGuard {
            listeners: Arc::clone(self),
            channel: channel.to_string(),
            id,
        }
    }

    /// Whether some registered listener on `channel` admits `target_identity`.
    pub fn admits(&self, channel: &str, target_identity: &str) -> bool {
        matches!(self.reach(channel, target_identity), Reach::Admitted)
    }

    /// Why an announcement can or cannot be carried on `channel`. Separating
    /// "nothing is connected" from "connected but refused" is what makes the
    /// silent-misconfiguration case (a bridge whose device was never linked
    /// to its owner) diagnosable: both fall back to Slack, but only one is a
    /// setup mistake, and from the user's chair they look identical.
    pub fn reach(&self, channel: &str, target_identity: &str) -> Reach {
        let inner = self.inner.read().expect("voice listener lock");
        let Some(listeners) = inner.get(channel).filter(|l| !l.is_empty()) else {
            return Reach::NoListener;
        };
        if listeners
            .iter()
            .any(|l| identity_admits(l.identity.as_deref(), target_identity))
        {
            Reach::Admitted
        } else {
            Reach::Refused {
                listeners: listeners.len(),
            }
        }
    }

    fn deregister(&self, channel: &str, id: u64) {
        let mut inner = self.inner.write().expect("voice listener lock");
        if let Some(listeners) = inner.get_mut(channel) {
            listeners.retain(|l| l.id != id);
            if listeners.is_empty() {
                inner.remove(channel);
            }
        }
        debug!(channel, "voice bridge deregistered");
    }
}

/// Deregisters its connection when dropped.
pub struct VoiceListenerGuard {
    listeners: Arc<VoiceListeners>,
    channel: String,
    id: u64,
}

impl Drop for VoiceListenerGuard {
    fn drop(&mut self) {
        self.listeners.deregister(&self.channel, self.id);
    }
}

/// The wire form of a cross-instance announcement request.
#[derive(Serialize, Deserialize)]
struct AnnounceRequest {
    channel: String,
    identity: String,
    text: String,
}

/// Delivers announcements to voice bridges, wherever their socket lands.
pub struct VoiceAnnouncer {
    listeners: Arc<VoiceListeners>,
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
    nats: Option<async_nats::Client>,
}

impl VoiceAnnouncer {
    pub fn new(
        listeners: Arc<VoiceListeners>,
        broadcast_tx: broadcast::Sender<BroadcastMessage>,
        nats: Option<async_nats::Client>,
    ) -> Self {
        Self {
            listeners,
            broadcast_tx,
            nats,
        }
    }

    /// Speak `text` on the bridge(s) registered for `channel` that admit
    /// `target_identity`. `Ok` means a bridge will receive it; `Err` means
    /// none is connected (anywhere) and the caller should fall back.
    pub async fn announce(&self, channel: &str, target_identity: &str, text: &str) -> Result<()> {
        match self.listeners.reach(channel, target_identity) {
            Reach::Admitted => {
                self.broadcast_tx
                    .send(BroadcastMessage::voice_announce(
                        channel,
                        target_identity,
                        text,
                    ))
                    .map_err(|_| eyre::eyre!("no gateway connections to carry the announcement"))?;
                return Ok(());
            }
            // Worth saying out loud, because it looks exactly like "the device
            // is off" and stays broken until someone links the device. Still
            // falls through: another instance may hold the bridge that IS
            // linked, and that one should get the chance to speak.
            Reach::Refused { listeners } => warn!(
                channel,
                listeners,
                identity = %target_identity,
                "a voice bridge is connected but none is linked to this identity — \
                 link the device to its owner (Users → the device's identity) or \
                 announcements will keep falling back to Slack"
            ),
            Reach::NoListener => {}
        }
        let Some(nats) = &self.nats else {
            return Err(eyre::eyre!("no voice bridge listening on '{channel}'"));
        };
        let request = serde_json::to_vec(&AnnounceRequest {
            channel: channel.to_string(),
            identity: target_identity.to_string(),
            text: text.to_string(),
        })?;
        // Only an instance whose registry admits the target replies; a timeout
        // is the "no bridge anywhere" answer.
        match tokio::time::timeout(
            ANNOUNCE_REPLY_TIMEOUT,
            nats.request(workforce_nats::subjects::voice_announce(), request.into()),
        )
        .await
        {
            Ok(Ok(_reply)) => Ok(()),
            Ok(Err(e)) => Err(eyre::eyre!(
                "no voice bridge listening on '{channel}' (announce request: {e})"
            )),
            Err(_) => Err(eyre::eyre!(
                "no voice bridge listening on '{channel}' (announce request timed out)"
            )),
        }
    }
}

/// Forward cross-instance announcement requests to locally-registered
/// bridges. One subscription per hub instance, spawned at startup in NATS
/// mode. Replies only when a local listener admits the target, so the
/// requester's timeout means "no bridge anywhere".
pub fn spawn_relay(
    nats: async_nats::Client,
    listeners: Arc<VoiceListeners>,
    broadcast_tx: broadcast::Sender<BroadcastMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut sub = match nats
            .subscribe(workforce_nats::subjects::voice_announce())
            .await
        {
            Ok(sub) => sub,
            Err(e) => {
                warn!(error = %e, "voice announce relay could not subscribe");
                return;
            }
        };
        while let Some(msg) = sub.next().await {
            let Ok(request) = serde_json::from_slice::<AnnounceRequest>(&msg.payload) else {
                warn!("dropping malformed voice announce request");
                continue;
            };
            if !listeners.admits(&request.channel, &request.identity) {
                continue;
            }
            // Only claim the announcement once it is actually on the local
            // bus. A listener that deregistered between the check and the
            // send leaves nothing to carry it, and replying `ok` there would
            // tell the requester it was spoken — losing the reminder instead
            // of falling back to Slack.
            if broadcast_tx
                .send(BroadcastMessage::voice_announce(
                    &request.channel,
                    &request.identity,
                    &request.text,
                ))
                .is_err()
            {
                warn!(
                    channel = %request.channel,
                    "voice listener vanished before the announcement was carried"
                );
                continue;
            }
            if let Some(reply) = msg.reply {
                let _ = nats.publish(reply, "ok".into()).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listeners() -> Arc<VoiceListeners> {
        Arc::new(VoiceListeners::default())
    }

    #[test]
    fn a_matching_identity_is_admitted() {
        assert!(identity_admits(Some("uid-1"), "uid-1"));
    }

    #[test]
    fn another_users_content_is_refused() {
        assert!(!identity_admits(Some("uid-2"), "uid-1"));
    }

    /// A paired device that never linked an identity must not hear anything —
    /// pairing alone is not authorization for someone's reminders, and there
    /// is no untargeted case that could let it through.
    #[test]
    fn an_unlinked_device_is_refused() {
        assert!(!identity_admits(None, "uid-1"));
    }

    /// The two fallback causes must stay distinguishable: a device that is
    /// off and a device that was never linked both end in Slack, but only
    /// the second is a setup mistake someone can fix.
    #[test]
    fn an_absent_bridge_and_an_unlinked_one_are_different_answers() {
        let listeners = listeners();
        assert_eq!(listeners.reach("voice-pe", "uid-1"), Reach::NoListener);

        let _unlinked = listeners.register("voice-pe", None);
        assert_eq!(
            listeners.reach("voice-pe", "uid-1"),
            Reach::Refused { listeners: 1 }
        );

        let _linked = listeners.register("voice-pe", Some("uid-1".into()));
        assert_eq!(listeners.reach("voice-pe", "uid-1"), Reach::Admitted);
    }

    /// An unlinked bridge must never be handed another identity's content —
    /// it falls back (via NATS when configured, else straight to Slack)
    /// rather than being spoken to.
    #[tokio::test]
    async fn an_unlinked_bridge_is_never_spoken_to() {
        let listeners = listeners();
        let _unlinked = listeners.register("voice-pe", None);
        let (tx, mut rx) = broadcast::channel(8);
        let announcer = VoiceAnnouncer::new(Arc::clone(&listeners), tx, None);

        let refused = announcer.announce("voice-pe", "uid-1", "hello").await;
        assert!(
            refused.is_err(),
            "an unlinked bridge cannot be spoken to, so delivery must fall back"
        );
        assert!(rx.try_recv().is_err(), "nothing may go on the bus");
    }

    #[test]
    fn registration_is_visible_until_the_guard_drops() {
        let listeners = listeners();
        assert!(!listeners.admits("voice-pe", "uid-1"));
        let guard = listeners.register("voice-pe", Some("uid-1".into()));
        assert!(listeners.admits("voice-pe", "uid-1"));
        assert!(!listeners.admits("other-channel", "uid-1"));
        drop(guard);
        assert!(!listeners.admits("voice-pe", "uid-1"));
    }

    #[test]
    fn dropping_one_guard_leaves_the_sibling_registered() {
        let listeners = listeners();
        let first = listeners.register("voice-pe", Some("uid-1".into()));
        let second = listeners.register("voice-pe", Some("uid-2".into()));
        drop(first);
        assert!(listeners.admits("voice-pe", "uid-2"));
        assert!(!listeners.admits("voice-pe", "uid-1"));
        drop(second);
        assert!(!listeners.admits("voice-pe", "uid-2"));
    }

    #[tokio::test]
    async fn a_local_listener_receives_the_broadcast() {
        let listeners = listeners();
        let _guard = listeners.register("voice-pe", Some("uid-1".into()));
        let (tx, mut rx) = broadcast::channel(8);
        let announcer = VoiceAnnouncer::new(Arc::clone(&listeners), tx, None);

        announcer
            .announce("voice-pe", "uid-1", "stretch your legs")
            .await
            .expect("a registered listener should be reachable");

        let msg = rx.try_recv().expect("the announcement should be queued");
        assert_eq!(msg.msg_type, "reminder");
        assert_eq!(msg.content, "stretch your legs");
        assert!(msg.session_id.is_empty());
        assert_eq!(msg.target_channel.as_deref(), Some("voice-pe"));
        assert_eq!(msg.target_identity.as_deref(), Some("uid-1"));
        assert!(msg.speakable);
    }

    #[tokio::test]
    async fn no_listener_and_no_nats_is_an_error() {
        let (tx, _rx) = broadcast::channel(8);
        let announcer = VoiceAnnouncer::new(listeners(), tx, None);
        let err = announcer
            .announce("voice-pe", "uid-1", "hello")
            .await
            .expect_err("nothing is listening");
        assert!(err.to_string().contains("no voice bridge"), "{err}");
    }

    /// The target fields route the message; they must never serialize toward
    /// a client, where another user's identity id would leak.
    #[test]
    fn target_fields_never_serialize() {
        let msg = BroadcastMessage::voice_announce("voice-pe", "uid-1", "hi");
        let json = serde_json::to_string(&msg).expect("serializes");
        assert!(!json.contains("target_channel"), "{json}");
        assert!(!json.contains("uid-1"), "{json}");
    }
}
