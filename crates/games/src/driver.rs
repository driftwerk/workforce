use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};

/// One AI move: ask `agent_id` to answer `prompt` for `room_id`. The driver is
/// the only seam that reaches the agent runtime, so the games crate stays pure
/// and the agent is driven in a locked-down, no-PII context (see the hub impl).
#[derive(Debug, Clone)]
pub struct AgentTurnRequest {
    pub agent_id: String,
    pub room_id: String,
    pub prompt: String,
}

/// Produces an in-character game move (text) from an agent.
#[async_trait]
pub trait AgentDriver: Send + Sync {
    async fn respond(&self, request: AgentTurnRequest) -> Result<String>;
}

/// A generated image: raw bytes + MIME type.
#[derive(Debug, Clone)]
pub struct GeneratedArt {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Draws an image for the given prompt using `agent_id`'s configured image model
/// (Reverse Pictionary). The hub impl reaches the agent runtime's image provider.
#[async_trait]
pub trait ImageGenerator: Send + Sync {
    async fn draw(&self, agent_id: &str, prompt: &str) -> Result<GeneratedArt>;
}

/// The authenticated human behind a connection. `id` keys the game server's own
/// per-player records; it is never threaded into the agent turn as a work identity.
#[derive(Debug, Clone)]
pub struct PlayerIdentity {
    pub id: String,
    pub display_name: String,
}

/// Validates a platform JWT into a player identity.
pub trait TokenVerifier: Send + Sync {
    fn verify(&self, token: &str) -> Option<PlayerIdentity>;
}

/// Shows `agent_id` a PNG drawing with `prompt` and returns its text guess
/// (Human Pictionary). The hub impl feeds the image to the agent's vision model
/// in the same locked-down, no-PII turn as the other ports. An empty reply means
/// "no guess".
#[async_trait]
pub trait VisionGuesser: Send + Sync {
    async fn guess(
        &self,
        agent_id: &str,
        room_id: &str,
        image_png: &[u8],
        prompt: &str,
    ) -> Result<String>;
}

/// One human-vs-bot outcome to persist: did `human_id` catch `agent_id` this
/// round? Keyed by the human's stable id and the configured agent (never the
/// in-game anonymous seat), so the running tally survives across games.
#[derive(Debug, Clone)]
pub struct PlayRecord {
    pub human_id: String,
    pub agent_id: String,
    pub caught: bool,
}

/// A human's cumulative record against one agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rivalry {
    pub caught: u32,
    pub fooled: u32,
    pub plays: u32,
}

/// Persists the human↔bot rivalry tally across games (the hub backs it with a
/// JSON file). The games crate only sees this port, never the storage.
pub trait RivalryStore: Send + Sync {
    fn record(&self, plays: &[PlayRecord]);
    fn rivalry(&self, human_id: &str, agent_id: &str) -> Rivalry;
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub struct EchoDriver;

    #[async_trait]
    impl AgentDriver for EchoDriver {
        async fn respond(&self, request: AgentTurnRequest) -> Result<String> {
            Ok(format!("[{}] {}", request.agent_id, request.prompt))
        }
    }

    pub struct AllowAllVerifier;

    impl TokenVerifier for AllowAllVerifier {
        fn verify(&self, token: &str) -> Option<PlayerIdentity> {
            (!token.is_empty()).then(|| PlayerIdentity {
                id: token.to_string(),
                display_name: format!("User {token}"),
            })
        }
    }

    pub struct StubArtist;

    #[async_trait]
    impl ImageGenerator for StubArtist {
        async fn draw(&self, _agent_id: &str, prompt: &str) -> Result<GeneratedArt> {
            Ok(GeneratedArt {
                bytes: format!("PNG:{prompt}").into_bytes(),
                mime: "image/png".to_string(),
            })
        }
    }

    pub struct StubGuesser;

    #[async_trait]
    impl VisionGuesser for StubGuesser {
        async fn guess(
            &self,
            _agent_id: &str,
            _room_id: &str,
            _image_png: &[u8],
            _prompt: &str,
        ) -> Result<String> {
            Ok(String::new())
        }
    }

    #[derive(Default)]
    pub struct InMemoryRivals {
        tally: std::sync::Mutex<std::collections::HashMap<(String, String), Rivalry>>,
    }

    impl RivalryStore for InMemoryRivals {
        fn record(&self, plays: &[PlayRecord]) {
            let mut tally = self.tally.lock().unwrap();
            for play in plays {
                let entry = tally
                    .entry((play.human_id.clone(), play.agent_id.clone()))
                    .or_default();
                entry.plays += 1;
                if play.caught {
                    entry.caught += 1;
                } else {
                    entry.fooled += 1;
                }
            }
        }

        fn rivalry(&self, human_id: &str, agent_id: &str) -> Rivalry {
            self.tally
                .lock()
                .unwrap()
                .get(&(human_id.to_string(), agent_id.to_string()))
                .cloned()
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::InMemoryRivals;
    use super::*;

    #[test]
    fn rivalry_store_accumulates_catches_and_fools() {
        let store = InMemoryRivals::default();
        store.record(&[
            PlayRecord {
                human_id: "alice".into(),
                agent_id: "penny_game".into(),
                caught: true,
            },
            PlayRecord {
                human_id: "alice".into(),
                agent_id: "bernard_game".into(),
                caught: false,
            },
        ]);
        store.record(&[PlayRecord {
            human_id: "alice".into(),
            agent_id: "penny_game".into(),
            caught: false,
        }]);

        let penny = store.rivalry("alice", "penny_game");
        assert_eq!((penny.caught, penny.fooled, penny.plays), (1, 1, 2));
        let bernard = store.rivalry("alice", "bernard_game");
        assert_eq!((bernard.caught, bernard.fooled, bernard.plays), (0, 1, 1));
        assert_eq!(store.rivalry("bob", "penny_game").plays, 0);
    }
}
