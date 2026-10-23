//! Hub adapter for the `workforce-games` ports: drives a locked-down game
//! agent in a Shared, no-PII context, and verifies player JWTs.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use eyre::{eyre, Result};
use parking_lot::Mutex;
use tracing::warn;
use workforce_common::settings::GamesConfig;
use workforce_common::trust::TrustLevel;
use workforce_games::{
    AgentDriver, AgentTurnRequest, GameManager, GamesState, GeneratedArt, ImageGenerator,
    PlayRecord, PlayerIdentity, Rivalry, RivalryStore, RoomConfig, TokenVerifier, VisionGuesser,
};

use crate::agent_registry::AgentRegistry;
use crate::gateway::GatewayState;
use crate::jwt::JwtConfig;

struct GamesBackend {
    registry: Arc<AgentRegistry>,
    jwt_config: JwtConfig,
}

#[async_trait]
impl AgentDriver for GamesBackend {
    async fn respond(&self, request: AgentTurnRequest) -> Result<String> {
        let service = self
            .registry
            .get(&request.agent_id)
            .ok_or_else(|| eyre!("game agent '{}' is not configured", request.agent_id))?;
        // `GAME_SANDBOX` runs this turn with no tools and no passive-memory write,
        // so any normal work agent plays safely without a separate locked-down
        // config. `CURRENT_CHANNEL_ID` is scoped around the whole call (mirroring
        // scheduler.rs) because the prompt is built before `process_message` sets
        // it; a `games:`-prefixed channel is Shared ⇒ no per-user PII and the
        // per-user/agent-shared recall block bails. The principal is non-real and
        // the player's name is carried only as game state.
        let channel_id = format!("games:{}", request.room_id);
        workforce_agent::runner::CURRENT_CHANNEL_ID
            .scope(
                channel_id.clone(),
                workforce_agent::runner::GAME_SANDBOX.scope(
                    true,
                    service.process_message(
                        "games",
                        "games",
                        &request.prompt,
                        None,
                        None,
                        Some(&channel_id),
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(TrustLevel::Untrusted),
                        None,
                    ),
                ),
            )
            .await
    }
}

#[async_trait]
impl VisionGuesser for GamesBackend {
    async fn guess(
        &self,
        agent_id: &str,
        room_id: &str,
        image_png: &[u8],
        prompt: &str,
    ) -> Result<String> {
        let service = self
            .registry
            .get(agent_id)
            .ok_or_else(|| eyre!("game agent '{agent_id}' is not configured"))?;
        // Same locked-down, no-PII, no-tools turn as `respond` — the drawing is
        // just an image content block. Needs a vision-capable model (or a vision
        // LLM); otherwise the image is stripped and the guess comes back empty.
        let channel_id = format!("games:{room_id}");
        let attachment = workforce_common::session::SessionAttachment::from_bytes(
            "drawing.png",
            "image/png",
            image_png,
        );
        workforce_agent::runner::CURRENT_CHANNEL_ID
            .scope(
                channel_id.clone(),
                workforce_agent::runner::GAME_SANDBOX.scope(
                    true,
                    service.process_message(
                        "games",
                        "games",
                        prompt,
                        None,
                        None,
                        Some(&channel_id),
                        None,
                        Some(vec![attachment]),
                        None,
                        None,
                        None,
                        Some(TrustLevel::Untrusted),
                        None,
                    ),
                ),
            )
            .await
    }
}

#[async_trait]
impl ImageGenerator for GamesBackend {
    async fn draw(&self, agent_id: &str, prompt: &str) -> Result<GeneratedArt> {
        let service = self
            .registry
            .get(agent_id)
            .ok_or_else(|| eyre!("game agent '{agent_id}' is not configured"))?;
        let cfg = service
            .image_config
            .read()
            .clone()
            .ok_or_else(|| eyre!("image generation is not configured for '{agent_id}'"))?;
        let provider = workforce_agent::llm::image::provider_from_config(
            &cfg,
            crate::credentials::image_keys(Some(&cfg), &service.vault, &service.global_vault),
        )?;
        let request = workforce_agent::llm::image::ImageRequest {
            prompt: prompt.to_string(),
            negative_prompt: None,
            size: workforce_agent::llm::image::ImageSize::parse(&cfg.size),
            n: 1,
            seed: None,
            reference_images: Vec::new(),
        };
        let image = provider
            .generate(&request)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| eyre!("image generation returned no images"))?;
        Ok(GeneratedArt {
            bytes: image.data,
            mime: image.media_type,
        })
    }
}

impl TokenVerifier for GamesBackend {
    fn verify(&self, token: &str) -> Option<PlayerIdentity> {
        let claims = crate::jwt::validate_token(&self.jwt_config, token).ok()?;
        Some(PlayerIdentity {
            id: claims.identity_id.unwrap_or_else(|| claims.sub.clone()),
            display_name: claims.sub,
        })
    }
}

/// Persists the human↔bot rivalry tally to `{data_dir}/games/rivalry.json`,
/// keyed `human_id → agent_id → tally`. Loaded once at boot, written
/// atomically after each recorded reveal.
pub(crate) struct JsonRivalryStore {
    path: PathBuf,
    tally: Mutex<HashMap<String, HashMap<String, Rivalry>>>,
}

/// The one tally kept under `data_dir`, loaded on first use: the games
/// router records into it and an identity lifecycle operation rewrites it, so
/// neither overwrites the other's change with a stale copy.
pub(crate) fn rivalry_store(data_dir: &Path) -> Arc<JsonRivalryStore> {
    static STORES: std::sync::LazyLock<Mutex<HashMap<PathBuf, Arc<JsonRivalryStore>>>> =
        std::sync::LazyLock::new(Default::default);
    Arc::clone(
        STORES
            .lock()
            .entry(data_dir.to_path_buf())
            .or_insert_with(|| Arc::new(JsonRivalryStore::load(data_dir))),
    )
}

impl JsonRivalryStore {
    /// How many bots anyone in `names` has a tally against.
    pub(crate) fn held(&self, names: &[String]) -> usize {
        let tally = self.tally.lock();
        names
            .iter()
            .filter_map(|n| tally.get(n))
            .map(HashMap::len)
            .sum()
    }

    /// How many bots each person has a tally against.
    pub(crate) fn humans(&self) -> std::collections::BTreeMap<String, usize> {
        self.tally
            .lock()
            .iter()
            .map(|(human, bots)| (human.clone(), bots.len()))
            .collect()
    }

    /// Add the tallies of everyone in `names` to `to`'s. Returns how many
    /// people's tallies moved.
    pub(crate) fn merge_into(&self, names: &[String], to: &str) -> usize {
        let mut tally = self.tally.lock();
        let moved: Vec<HashMap<String, Rivalry>> = names
            .iter()
            .filter(|n| n.as_str() != to)
            .filter_map(|n| tally.remove(n))
            .collect();
        let count = moved.len();
        let theirs = tally.entry(to.to_string()).or_default();
        for (agent, r) in moved.into_iter().flatten() {
            let entry = theirs.entry(agent).or_default();
            entry.plays += r.plays;
            entry.caught += r.caught;
            entry.fooled += r.fooled;
        }
        if theirs.is_empty() {
            tally.remove(to);
        }
        if count > 0 {
            self.persist(&tally);
        }
        count
    }

    /// Remove the tallies of everyone in `names`. Returns how many went.
    pub(crate) fn forget(&self, names: &[String]) -> usize {
        let mut tally = self.tally.lock();
        let count = names.iter().filter(|n| tally.remove(*n).is_some()).count();
        if count > 0 {
            self.persist(&tally);
        }
        count
    }

    fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("games").join("rivalry.json");
        let tally = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            tally: Mutex::new(tally),
        }
    }

    fn persist(&self, tally: &HashMap<String, HashMap<String, Rivalry>>) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(error = %e, "[games] could not create rivalry data dir");
            return;
        }
        match serde_json::to_vec_pretty(tally) {
            Ok(bytes) => {
                if let Err(e) = atomic_write(&self.path, &bytes) {
                    warn!(error = %e, "[games] could not persist rivalry tally");
                }
            }
            Err(e) => warn!(error = %e, "[games] could not serialise rivalry tally"),
        }
    }
}

impl RivalryStore for JsonRivalryStore {
    fn record(&self, plays: &[PlayRecord]) {
        let mut tally = self.tally.lock();
        for play in plays {
            let entry = tally
                .entry(play.human_id.clone())
                .or_default()
                .entry(play.agent_id.clone())
                .or_default();
            entry.plays += 1;
            if play.caught {
                entry.caught += 1;
            } else {
                entry.fooled += 1;
            }
        }
        self.persist(&tally);
    }

    fn rivalry(&self, human_id: &str, agent_id: &str) -> Rivalry {
        self.tally
            .lock()
            .get(human_id)
            .and_then(|agents| agents.get(agent_id))
            .cloned()
            .unwrap_or_default()
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| eyre!("no parent directory for {}", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.flush()?;
    tmp.persist(path)
        .map_err(|e| eyre!("persist {}: {e}", path.display()))?;
    Ok(())
}

/// Nest the `/games` router onto `app` when games are enabled and JWT auth is
/// configured. A no-op otherwise.
pub fn mount(
    app: Router,
    state: &Arc<GatewayState>,
    games: &GamesConfig,
    data_dir: &Path,
) -> Router {
    if !games.enabled {
        return app;
    }
    let Some(jwt_config) = state.jwt_config.clone() else {
        warn!("[games] enabled but JWT auth is not configured; games not mounted");
        return app;
    };
    let backend = Arc::new(GamesBackend {
        registry: Arc::clone(&state.registry),
        jwt_config,
    });
    let rivalry = rivalry_store(data_dir);
    let manager = Arc::new(GameManager::new(
        Arc::clone(&backend) as Arc<dyn AgentDriver>,
        Arc::clone(&backend) as Arc<dyn ImageGenerator>,
        Arc::clone(&backend) as Arc<dyn VisionGuesser>,
        rivalry as Arc<dyn RivalryStore>,
        games.agent_players.clone(),
        RoomConfig::default(),
    ));
    let games_state = Arc::new(GamesState {
        manager,
        verifier: Arc::clone(&backend) as Arc<dyn TokenVerifier>,
    });
    app.nest("/games", workforce_games::router(games_state))
}

#[cfg(test)]
mod rivalry_tests {
    use super::*;

    fn play(human: &str, agent: &str, caught: bool) -> PlayRecord {
        PlayRecord {
            human_id: human.into(),
            agent_id: agent.into(),
            caught,
        }
    }

    #[test]
    fn a_tally_is_merged_into_another_person_and_forgotten_on_disk_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = rivalry_store(dir.path());
        store.record(&[play("alice", "bot", true), play("bob", "bot", false)]);
        let alice = vec!["alice".to_string()];

        assert_eq!(store.held(&alice), 1);
        assert_eq!(
            store.humans(),
            std::collections::BTreeMap::from([("alice".to_string(), 1), ("bob".to_string(), 1)])
        );
        assert_eq!(store.merge_into(&alice, "bob"), 1);
        let merged = store.rivalry("bob", "bot");
        assert_eq!((merged.plays, merged.caught, merged.fooled), (2, 1, 1));
        assert_eq!(store.held(&alice), 0);

        assert_eq!(store.forget(&["bob".to_string()]), 1);
        let reloaded = JsonRivalryStore::load(dir.path());
        assert_eq!(reloaded.rivalry("bob", "bot").plays, 0, "persisted");
        assert!(Arc::ptr_eq(&store, &rivalry_store(dir.path())));
    }
}
