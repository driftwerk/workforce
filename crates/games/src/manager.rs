use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tracing::warn;

use crate::balderdash::Balderdash;
use crate::caption_battle::CaptionBattle;
use crate::driver::{
    AgentDriver, AgentTurnRequest, ImageGenerator, PlayRecord, PlayerIdentity, RivalryStore,
    VisionGuesser,
};
use crate::game::{
    AiSeat, AiWork, Difficulty, Game, GameAction, GameError, RoomConfig, TextKind,
    DEFAULT_AI_FALLBACK,
};
use crate::human_pictionary::HumanPictionary;
use crate::protocol::{
    BalderdashPack, ClientGameMessage, GameKind, Rivalries, RivalryRow, RoomSummary, RoomView,
    ServerGameMessage,
};
use crate::reverse_pictionary::ReversePictionary;
use crate::spot_the_bot::SpotTheBot;

const BROADCAST_CAPACITY: usize = 64;
/// Reap a room (and stop its ticker) after this many seconds of being empty or
/// finished, so abandoned lobbies and ended games don't accumulate.
const ROOM_REAP_SECS: u32 = 90;
/// Most bots a single room will seat.
const MAX_BOTS: usize = 3;

struct RoomHandle {
    game: Mutex<Box<dyn Game>>,
    tx: broadcast::Sender<ServerGameMessage>,
    /// Keys of the AI work already dispatched (`AiWork::key`), so each bot's
    /// answer / jab / the image fires exactly once even as other actions land.
    dispatched: Mutex<HashSet<(u32, String)>>,
}

pub struct GameManager {
    rooms: RwLock<HashMap<String, Arc<RoomHandle>>>,
    driver: Arc<dyn AgentDriver>,
    image_gen: Arc<dyn ImageGenerator>,
    vision: Arc<dyn VisionGuesser>,
    rivalry: Arc<dyn RivalryStore>,
    agent_players: Vec<String>,
    config: RoomConfig,
}

pub struct JoinOutcome {
    pub player_id: String,
    pub events: broadcast::Receiver<ServerGameMessage>,
}

#[derive(Debug)]
pub enum JoinError {
    NotFound,
    Rejected(String),
}

#[derive(Debug)]
pub enum DrawingError {
    NotFound,
    NotDrawer,
    WrongPhase,
}

impl GameManager {
    pub fn new(
        driver: Arc<dyn AgentDriver>,
        image_gen: Arc<dyn ImageGenerator>,
        vision: Arc<dyn VisionGuesser>,
        rivalry: Arc<dyn RivalryStore>,
        agent_players: Vec<String>,
        config: RoomConfig,
    ) -> Self {
        Self {
            rooms: RwLock::new(HashMap::new()),
            driver,
            image_gen,
            vision,
            rivalry,
            agent_players,
            config,
        }
    }

    pub fn create_room(
        self: &Arc<Self>,
        host: &PlayerIdentity,
        kind: GameKind,
        rounds: Option<u32>,
        bots: Option<u8>,
        difficulty: Option<Difficulty>,
        pack: Option<BalderdashPack>,
    ) -> String {
        let id = self.fresh_id();
        let mut config = self.config.clone();
        if let Some(rounds) = rounds {
            config.total_rounds = rounds.clamp(1, 10);
        }
        if let Some(difficulty) = difficulty {
            config.difficulty = difficulty;
        }
        let seats = self.seats(bots);
        let game: Box<dyn Game> = match kind {
            GameKind::SpotTheBot => Box::new(SpotTheBot::new(id.clone(), host, config, seats)),
            GameKind::Balderdash => Box::new(Balderdash::new(
                id.clone(),
                host,
                config,
                seats,
                pack.unwrap_or_default(),
            )),
            GameKind::CaptionBattle => {
                Box::new(CaptionBattle::new(id.clone(), host, config, seats))
            }
            GameKind::HumanPictionary => Box::new(HumanPictionary::new(
                id.clone(),
                host,
                config,
                seats.into_iter().next(),
            )),
            GameKind::ReversePictionary => Box::new(ReversePictionary::new(
                id.clone(),
                host,
                config,
                seats.into_iter().next(),
            )),
        };
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let handle = Arc::new(RoomHandle {
            game: Mutex::new(game),
            tx,
            dispatched: Mutex::new(HashSet::new()),
        });
        self.rooms.write().insert(id.clone(), handle);
        self.spawn_ticker(id.clone());
        id
    }

    /// Number of currently-live rooms. Used to cap create-spam (audit G2);
    /// idle/finished rooms self-reap after `ROOM_REAP_SECS`.
    pub fn active_room_count(&self) -> usize {
        self.rooms.read().len()
    }

    pub fn list_rooms(&self) -> Vec<RoomSummary> {
        self.rooms
            .read()
            .values()
            .filter_map(|handle| {
                let game = handle.game.lock();
                (!game.is_finished()).then(|| game.summary())
            })
            .collect()
    }

    pub fn join(&self, room_id: &str, identity: &PlayerIdentity) -> Result<JoinOutcome, JoinError> {
        let handle = self
            .rooms
            .read()
            .get(room_id)
            .cloned()
            .ok_or(JoinError::NotFound)?;
        let events = handle.tx.subscribe();
        let (player_id, view) = {
            let mut game = handle.game.lock();
            let player_id = game
                .join(identity)
                .map_err(|e| JoinError::Rejected(e.to_string()))?;
            (player_id, game.view())
        };
        self.broadcast(&handle, ServerGameMessage::state(view));
        Ok(JoinOutcome { player_id, events })
    }

    pub fn on_disconnect(self: &Arc<Self>, room_id: &str, player_id: &str) {
        let Some(handle) = self.rooms.read().get(room_id).cloned() else {
            return;
        };
        // A leaving player can be the last one a phase was waiting on, so nudge
        // the round forward rather than stalling until the deadline.
        let (view, work, plays) = {
            let mut game = handle.game.lock();
            game.set_connected(player_id, false);
            game.nudge(now());
            (game.view(), game.pending_work(), game.take_round_plays())
        };
        self.settle(&handle, room_id, view, work, plays);
    }

    /// The current round's generated image for a room, if any.
    pub fn room_art(&self, room_id: &str) -> Option<(Vec<u8>, String)> {
        let handle = self.rooms.read().get(room_id).cloned()?;
        let game = handle.game.lock();
        let art = game.art()?;
        Some((art.bytes.to_vec(), art.mime.to_string()))
    }

    pub async fn handle_message(
        self: &Arc<Self>,
        room_id: &str,
        player_id: &str,
        msg: ClientGameMessage,
    ) -> Option<ServerGameMessage> {
        let handle = self.rooms.read().get(room_id).cloned()?;
        let action = match msg {
            ClientGameMessage::Ping => return Some(ServerGameMessage::Pong),
            // Replied privately to the requesting socket only — never broadcast.
            // The drawer gets their shortlist while choosing, else their secret word.
            ClientGameMessage::RequestWord => {
                let game = handle.game.lock();
                let choices = game.choices_for(player_id);
                return if choices.is_empty() {
                    game.word_for(player_id)
                        .map(|word| ServerGameMessage::YourWord {
                            word: word.to_string(),
                        })
                } else {
                    Some(ServerGameMessage::WordChoices { words: choices })
                };
            }
            ClientGameMessage::Start => GameAction::Start,
            ClientGameMessage::Submit { text } => GameAction::Submit(text),
            ClientGameMessage::Vote { target } => GameAction::Vote(target),
            ClientGameMessage::Guess { text } => GameAction::Guess(text),
            ClientGameMessage::ChooseWord { word } => GameAction::Choose(word),
            ClientGameMessage::Continue => GameAction::Continue,
        };
        self.act(&handle, room_id, player_id, action)
    }

    /// A drawer's uploaded PNG snapshot (Human Pictionary, via HTTP POST). Mirrors
    /// `act`: mutate under the lock, then broadcast + dispatch any owed vision work.
    pub fn apply_drawing(
        self: &Arc<Self>,
        room_id: &str,
        drawer_id: &str,
        png: Vec<u8>,
        round: Option<u32>,
        seq: Option<u32>,
    ) -> Result<(), DrawingError> {
        let handle = self
            .rooms
            .read()
            .get(room_id)
            .cloned()
            .ok_or(DrawingError::NotFound)?;
        let outcome = {
            let mut game = handle.game.lock();
            game.apply_drawing(drawer_id, png, round, seq, now())
                .map(|()| (game.view(), game.pending_work(), game.take_round_plays()))
        };
        match outcome {
            Ok((view, work, plays)) => {
                self.settle(&handle, room_id, view, work, plays);
                Ok(())
            }
            Err(GameError::NotDrawer) => Err(DrawingError::NotDrawer),
            Err(_) => Err(DrawingError::WrongPhase),
        }
    }

    fn act(
        self: &Arc<Self>,
        handle: &Arc<RoomHandle>,
        room_id: &str,
        player_id: &str,
        action: GameAction,
    ) -> Option<ServerGameMessage> {
        let outcome = {
            let mut game = handle.game.lock();
            game.act(player_id, action, now())
                .map(|()| (game.view(), game.pending_work(), game.take_round_plays()))
        };
        match outcome {
            Ok((view, work, plays)) => {
                self.settle(handle, room_id, view, work, plays);
                None
            }
            Err(e) => Some(ServerGameMessage::Error {
                message: e.to_string(),
            }),
        }
    }

    /// After a state mutation: broadcast the new view, dispatch any AI work the
    /// round now owes, and persist + refresh rivalries.
    fn settle(
        self: &Arc<Self>,
        handle: &Arc<RoomHandle>,
        room_id: &str,
        view: RoomView,
        work: Vec<AiWork>,
        plays: Vec<PlayRecord>,
    ) {
        self.broadcast(handle, ServerGameMessage::state(view));
        self.maybe_dispatch(handle, room_id, work);
        self.record_and_refresh(handle, plays);
    }

    /// Persist the round's outcomes and feed the refreshed tallies back into the
    /// room so the reveal can show each player's running record.
    fn record_and_refresh(&self, handle: &Arc<RoomHandle>, plays: Vec<PlayRecord>) {
        if plays.is_empty() {
            return;
        }
        self.rivalry.record(&plays);
        let rivalries = self.snapshot(&plays);
        let view = {
            let mut game = handle.game.lock();
            game.set_rivalries(rivalries);
            game.view()
        };
        self.broadcast(handle, ServerGameMessage::state(view));
    }

    /// Build the post-round rivalry tallies for the view, one row per distinct
    /// (human, bot) pair that just played.
    fn snapshot(&self, plays: &[PlayRecord]) -> Rivalries {
        let mut rivalries: Rivalries = HashMap::new();
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        for play in plays {
            if !seen.insert((&play.human_id, &play.agent_id)) {
                continue;
            }
            let tally = self.rivalry.rivalry(&play.human_id, &play.agent_id);
            rivalries
                .entry(play.human_id.clone())
                .or_default()
                .push(RivalryRow {
                    persona: persona_label(&play.agent_id),
                    caught: tally.caught,
                    fooled: tally.fooled,
                    plays: tally.plays,
                });
        }
        rivalries
    }

    /// Run each piece of AI work the round owes, exactly once.
    fn maybe_dispatch(
        self: &Arc<Self>,
        handle: &Arc<RoomHandle>,
        room_id: &str,
        work: Vec<AiWork>,
    ) {
        for item in work {
            if !handle.dispatched.lock().insert(item.key()) {
                continue;
            }
            match item {
                AiWork::Text {
                    round,
                    player_id,
                    agent_id,
                    kind,
                    prompt,
                } => self.spawn_text(
                    room_id.to_string(),
                    agent_id,
                    player_id,
                    kind,
                    prompt,
                    round,
                ),
                AiWork::Image {
                    round,
                    agent_id,
                    prompt,
                } => self.spawn_image(room_id.to_string(), agent_id, prompt, round),
                AiWork::Vision {
                    round,
                    agent_id,
                    player_id,
                    prompt,
                    ..
                } => self.spawn_vision(room_id.to_string(), agent_id, player_id, prompt, round),
            }
        }
    }

    /// Show the current drawing to the AI guesser and apply its guess. The PNG is
    /// read from `Game::art` under the lock at dispatch time (never carried in the
    /// work item).
    fn spawn_vision(
        self: &Arc<Self>,
        room_id: String,
        agent_id: String,
        player_id: String,
        prompt: String,
        round: u32,
    ) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let png = manager
                .rooms
                .read()
                .get(&room_id)
                .and_then(|h| h.game.lock().art().map(|a| a.bytes.to_vec()));
            let Some(png) = png else {
                return;
            };
            let guess = match manager
                .vision
                .guess(&agent_id, &room_id, &png, &prompt)
                .await
            {
                Ok(text) => text,
                Err(e) => {
                    warn!(error = %e, agent = %agent_id, "AI vision guess failed");
                    String::new()
                }
            };
            manager.apply_work(&room_id, |game| {
                game.apply_guess(round, &player_id, guess, now())
            });
        });
    }

    /// Seat up to `bots` (default 1) distinct configured agents, capped by both
    /// `MAX_BOTS` and the number of agents available.
    fn seats(&self, bots: Option<u8>) -> Vec<AiSeat> {
        let want = (bots.unwrap_or(1) as usize)
            .clamp(1, MAX_BOTS)
            .min(self.agent_players.len());
        self.agent_players
            .iter()
            .take(want)
            .map(|agent_id| AiSeat {
                agent_id: agent_id.clone(),
                persona: persona_label(agent_id),
            })
            .collect()
    }

    /// The most bots the lobby may offer for a room (for `GET /api/config`).
    pub fn max_bots(&self) -> usize {
        self.agent_players.len().min(MAX_BOTS)
    }

    fn broadcast(&self, handle: &RoomHandle, msg: ServerGameMessage) {
        let _ = handle.tx.send(msg);
    }

    fn fresh_id(&self) -> String {
        loop {
            let code = uuid::Uuid::new_v4().simple().to_string()[..6].to_uppercase();
            if !self.rooms.read().contains_key(&code) {
                return code;
            }
        }
    }

    fn spawn_text(
        self: &Arc<Self>,
        room_id: String,
        agent_id: String,
        player_id: String,
        kind: TextKind,
        prompt: String,
        round: u32,
    ) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let request = AgentTurnRequest {
                agent_id: agent_id.clone(),
                room_id: room_id.clone(),
                prompt,
            };
            // An answer always needs *something* (else the bot is obvious), and
            // the game picks an in-genre filler; a failed jab is simply dropped.
            let fallback = || match kind {
                TextKind::Answer => manager
                    .rooms
                    .read()
                    .get(&room_id)
                    .map(|h| h.game.lock().answer_fallback())
                    .unwrap_or_else(|| DEFAULT_AI_FALLBACK.to_string()),
                TextKind::Jab => String::new(),
            };
            let text = match manager.driver.respond(request).await {
                Ok(text) if !text.trim().is_empty() => text,
                Ok(_) => fallback(),
                Err(e) => {
                    warn!(error = %e, agent = %agent_id, "AI game move failed; using fallback");
                    fallback()
                }
            };
            manager.apply_work(&room_id, |game| {
                game.apply_text(round, &player_id, kind, text, now())
            });
        });
    }

    fn spawn_image(
        self: &Arc<Self>,
        room_id: String,
        agent_id: String,
        prompt: String,
        round: u32,
    ) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            match manager.image_gen.draw(&agent_id, &prompt).await {
                Ok(art) => {
                    manager.apply_work(&room_id, |game| game.apply_image(round, art, now()));
                }
                Err(e) => {
                    warn!(error = %e, agent = %agent_id, "AI drawing failed");
                    manager.apply_work(&room_id, |game| game.fail_work(round, now()));
                }
            }
        });
    }

    fn apply_work(&self, room_id: &str, f: impl FnOnce(&mut dyn Game)) {
        let Some(handle) = self.rooms.read().get(room_id).cloned() else {
            return;
        };
        let view = {
            let mut game = handle.game.lock();
            f(game.as_mut());
            game.view()
        };
        self.broadcast(&handle, ServerGameMessage::state(view));
    }

    fn spawn_ticker(self: &Arc<Self>, room_id: String) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            let mut idle: u32 = 0;
            loop {
                interval.tick().await;
                let Some(handle) = manager.rooms.read().get(&room_id).cloned() else {
                    return;
                };
                let (progressed, active) = {
                    let mut game = handle.game.lock();
                    let progressed = game
                        .advance(now())
                        .then(|| (game.view(), game.pending_work(), game.take_round_plays()));
                    (
                        progressed,
                        game.connected_humans() > 0 && !game.is_finished(),
                    )
                };
                if let Some((view, work, plays)) = progressed {
                    manager.settle(&handle, &room_id, view, work, plays);
                }
                idle = if active { 0 } else { idle + 1 };
                if idle >= ROOM_REAP_SECS {
                    manager.rooms.write().remove(&room_id);
                    return;
                }
            }
        });
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn persona_label(agent_id: &str) -> String {
    let base = agent_id
        .strip_suffix("_game")
        .or_else(|| agent_id.strip_suffix("-game"))
        .unwrap_or(agent_id);
    base.split(['_', '-'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::testing::{EchoDriver, InMemoryRivals, StubArtist, StubGuesser};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn id(name: &str) -> PlayerIdentity {
        PlayerIdentity {
            id: name.to_string(),
            display_name: name.to_uppercase(),
        }
    }

    struct CountingDriver(Arc<AtomicUsize>);

    #[async_trait]
    impl AgentDriver for CountingDriver {
        async fn respond(&self, _request: AgentTurnRequest) -> eyre::Result<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("blending in".to_string())
        }
    }

    fn manager() -> Arc<GameManager> {
        Arc::new(GameManager::new(
            Arc::new(EchoDriver),
            Arc::new(StubArtist),
            Arc::new(StubGuesser),
            Arc::new(InMemoryRivals::default()),
            vec!["penny_game".into()],
            RoomConfig::default(),
        ))
    }

    #[test]
    fn persona_label_strips_suffix_and_titlecases() {
        assert_eq!(persona_label("penny_game"), "Penny");
        assert_eq!(persona_label("atlas-game"), "Atlas");
        assert_eq!(persona_label("acme_bot"), "Acme Bot");
    }

    #[tokio::test]
    async fn create_join_and_list() {
        let mgr = manager();
        let room_id = mgr.create_room(
            &id("alice"),
            GameKind::SpotTheBot,
            Some(2),
            None,
            None,
            None,
        );
        let outcome = mgr.join(&room_id, &id("alice")).expect("join");
        assert_eq!(outcome.player_id, "alice");
        let rooms = mgr.list_rooms();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].kind, GameKind::SpotTheBot);
        assert_eq!(rooms[0].host, "ALICE");
    }

    #[tokio::test]
    async fn unknown_room_join_fails() {
        let mgr = manager();
        assert!(matches!(
            mgr.join("NOPE", &id("alice")).err(),
            Some(JoinError::NotFound)
        ));
    }

    #[tokio::test]
    async fn spot_the_bot_start_is_host_gated_and_has_no_art() {
        let mgr = manager();
        let room_id = mgr.create_room(&id("alice"), GameKind::SpotTheBot, None, None, None, None);
        mgr.join(&room_id, &id("alice")).unwrap();
        mgr.join(&room_id, &id("bob")).unwrap();
        assert!(matches!(
            mgr.handle_message(&room_id, "bob", ClientGameMessage::Start)
                .await,
            Some(ServerGameMessage::Error { .. })
        ));
        assert!(mgr
            .handle_message(&room_id, "alice", ClientGameMessage::Start)
            .await
            .is_none());
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Bots answer only after the humans do, and Spot the Bot never draws.
        assert!(mgr.room_art(&room_id).is_none());
    }

    #[tokio::test]
    async fn pictionary_draws_after_start_and_serves_art() {
        let mgr = manager();
        let room_id = mgr.create_room(
            &id("alice"),
            GameKind::ReversePictionary,
            None,
            None,
            None,
            None,
        );
        mgr.join(&room_id, &id("alice")).unwrap();
        mgr.handle_message(&room_id, "alice", ClientGameMessage::Start)
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let art = mgr.room_art(&room_id).expect("art generated");
        assert_eq!(art.1, "image/png");
    }

    #[tokio::test]
    async fn each_bot_dispatched_once_after_humans_answer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mgr = Arc::new(GameManager::new(
            Arc::new(CountingDriver(Arc::clone(&calls))),
            Arc::new(StubArtist),
            Arc::new(StubGuesser),
            Arc::new(InMemoryRivals::default()),
            vec!["penny_game".into(), "bernard_game".into()],
            RoomConfig::default(),
        ));
        let room = mgr.create_room(
            &id("alice"),
            GameKind::SpotTheBot,
            None,
            Some(2),
            None,
            None,
        );
        for p in ["alice", "bob"] {
            mgr.join(&room, &id(p)).unwrap();
        }
        mgr.handle_message(&room, "alice", ClientGameMessage::Start)
            .await;
        // Bots owe nothing until every human has answered.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Once the last human answers, each of the two bots is dispatched once.
        mgr.handle_message(
            &room,
            "alice",
            ClientGameMessage::Submit { text: "a".into() },
        )
        .await;
        mgr.handle_message(&room, "bob", ClientGameMessage::Submit { text: "b".into() })
            .await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rivalry_recorded_at_reveal() {
        let store = Arc::new(InMemoryRivals::default());
        let mgr = Arc::new(GameManager::new(
            Arc::new(EchoDriver),
            Arc::new(StubArtist),
            Arc::new(StubGuesser),
            Arc::clone(&store) as Arc<dyn RivalryStore>,
            vec!["penny_game".into()],
            RoomConfig::default(),
        ));
        let room = mgr.create_room(
            &id("alice"),
            GameKind::SpotTheBot,
            None,
            Some(1),
            None,
            None,
        );
        let mut events = mgr.join(&room, &id("alice")).unwrap().events;
        mgr.join(&room, &id("bob")).unwrap();
        mgr.handle_message(&room, "alice", ClientGameMessage::Start)
            .await;
        mgr.handle_message(
            &room,
            "alice",
            ClientGameMessage::Submit { text: "a".into() },
        )
        .await;
        mgr.handle_message(&room, "bob", ClientGameMessage::Submit { text: "b".into() })
            .await;
        // Let the bot answer (EchoDriver) so the room reaches Voting.
        tokio::time::sleep(Duration::from_millis(60)).await;
        // A client votes with the per-round opaque token from the view, picked
        // here by answer text. Both humans vote a human, so the bot fools both.
        let view = latest_view(&mut events);
        let token = |text: &str| match &view.game {
            crate::protocol::GameView::SpotTheBot { responses, .. } => responses
                .iter()
                .find(|r| r.text == text)
                .map(|r| r.player_id.clone())
                .expect("answer present"),
            _ => panic!("wrong view"),
        };
        mgr.handle_message(
            &room,
            "alice",
            ClientGameMessage::Vote { target: token("b") },
        )
        .await;
        mgr.handle_message(&room, "bob", ClientGameMessage::Vote { target: token("a") })
            .await;
        let rivalry = store.rivalry("alice", "penny_game");
        assert_eq!((rivalry.plays, rivalry.fooled, rivalry.caught), (1, 1, 0));
    }

    /// Drain a room's broadcast to its newest snapshot — how a freshly-attached
    /// client renders, and how a test reads the per-round vote tokens.
    fn latest_view(events: &mut broadcast::Receiver<ServerGameMessage>) -> RoomView {
        let mut latest = None;
        loop {
            match events.try_recv() {
                Ok(ServerGameMessage::State(view)) => latest = Some(*view),
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        latest.expect("a broadcast state")
    }

    #[tokio::test]
    async fn ping_returns_pong() {
        let mgr = manager();
        let room_id = mgr.create_room(&id("alice"), GameKind::SpotTheBot, None, None, None, None);
        assert!(matches!(
            mgr.handle_message(&room_id, "alice", ClientGameMessage::Ping)
                .await,
            Some(ServerGameMessage::Pong)
        ));
    }
}
