use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::driver::{GeneratedArt, PlayRecord, PlayerIdentity};
use crate::protocol::{PlayerView, Rivalries, RoomSummary, RoomView, ScoreRow};

pub type PlayerId = String;

/// Lifecycle phases across all games. Each game uses the subset it needs:
/// Spot the Bot uses Collecting/Voting; Reverse Pictionary uses Drawing/Guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Lobby,
    Collecting,
    Voting,
    /// The drawer is picking their word from a private shortlist (Human Pictionary).
    Choosing,
    Drawing,
    Guessing,
    Reveal,
    GameOver,
}

/// How hard a game plays — the bot blends harder and the drawings get trickier.
/// Each game interprets it for the parts where difficulty makes sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Difficulty {
    Easy,
    #[default]
    Normal,
    Hard,
    VeryHard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerKind {
    Human,
    Ai { agent_id: String, persona: String },
}

#[derive(Debug, Clone)]
pub struct Player {
    pub id: PlayerId,
    pub display_name: String,
    pub label: String,
    pub avatar: String,
    pub kind: PlayerKind,
    pub connected: bool,
    pub score: u32,
}

impl Player {
    pub fn is_ai(&self) -> bool {
        matches!(self.kind, PlayerKind::Ai { .. })
    }

    pub fn persona(&self) -> Option<&str> {
        match &self.kind {
            PlayerKind::Ai { persona, .. } => Some(persona),
            PlayerKind::Human => None,
        }
    }
}

/// The AI player/artist a game seats when it starts.
#[derive(Debug, Clone)]
pub struct AiSeat {
    pub agent_id: String,
    pub persona: String,
}

#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub total_rounds: u32,
    pub collect_secs: i64,
    pub vote_secs: i64,
    pub choose_secs: i64,
    pub draw_secs: i64,
    pub guess_secs: i64,
    pub reveal_secs: i64,
    pub min_humans: usize,
    pub difficulty: Difficulty,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            total_rounds: 3,
            collect_secs: 45,
            vote_secs: 30,
            choose_secs: 15,
            draw_secs: 75,
            guess_secs: 40,
            reveal_secs: 16,
            min_humans: 1,
            difficulty: Difficulty::Normal,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GameError {
    NotHost,
    NotLobby,
    WrongPhase,
    NotAPlayer,
    GameInProgress,
    NeedMoreHumans(usize),
    NoAiAvailable,
    SelfVote,
    UnknownTarget,
    NotDrawer,
}

impl fmt::Display for GameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::NotHost => "Only the host can start the game".into(),
            Self::NotLobby => "The game has already started".into(),
            Self::WrongPhase => "You can't do that right now".into(),
            Self::NotAPlayer => "You are not in this game".into(),
            Self::GameInProgress => "This game is already in progress".into(),
            Self::NeedMoreHumans(n) => format!("Need at least {n} players to start"),
            Self::NoAiAvailable => "No AI player is configured".into(),
            Self::SelfVote => "You can't vote for yourself".into(),
            Self::UnknownTarget => "Unknown player".into(),
            Self::NotDrawer => "Only the drawer can do that".into(),
        };
        write!(f, "{msg}")
    }
}

/// One thing a player tries to do. Each game accepts the subset it understands.
#[derive(Debug, Clone)]
pub enum GameAction {
    Start,
    Submit(String),
    Vote(PlayerId),
    Guess(String),
    /// The drawer commits to one of their offered words (Human Pictionary).
    Choose(String),
    /// Host skips the reveal timer to the next round (or game-end).
    Continue,
}

/// Which kind of text turn a bot owes the round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Answer,
    Jab,
}

/// Work the engine must perform for the current round, then feed back via
/// [`Game::apply_text`] / [`Game::apply_image`]. A round may owe several (one
/// per bot, plus reveal jabs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiWork {
    Text {
        round: u32,
        player_id: PlayerId,
        agent_id: String,
        kind: TextKind,
        prompt: String,
    },
    Image {
        round: u32,
        agent_id: String,
        prompt: String,
    },
    /// The AI looks at the current drawing (read from `Game::art` at dispatch)
    /// and guesses; `snapshot` keys it so each checkpoint fires once.
    Vision {
        round: u32,
        snapshot: u32,
        player_id: PlayerId,
        agent_id: String,
        prompt: String,
    },
}

impl AiWork {
    /// A per-round identity used to dispatch each piece of work exactly once.
    pub fn key(&self) -> (u32, String) {
        match self {
            Self::Text {
                round,
                player_id,
                kind,
                ..
            } => {
                let tag = match kind {
                    TextKind::Answer => "answer",
                    TextKind::Jab => "jab",
                };
                (*round, format!("{tag}:{player_id}"))
            }
            Self::Vision {
                round, snapshot, ..
            } => (*round, format!("guess:{snapshot}")),
            Self::Image { round, .. } => (*round, "image".to_string()),
        }
    }
}

/// The current round's generated image, for the art HTTP endpoint.
pub struct RoundArt<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
}

/// Generic filler for a bot answer that came back empty (overridden per game).
pub(crate) const DEFAULT_AI_FALLBACK: &str = "honestly not sure, ask me again after coffee ☕";

/// A single game's state machine. Shared infrastructure (rooms, broadcast,
/// timer, websocket, AI/image ports) lives in the manager and drives this.
pub trait Game: Send {
    fn join(&mut self, identity: &PlayerIdentity) -> Result<PlayerId, GameError>;
    fn set_connected(&mut self, id: &str, connected: bool);
    fn act(&mut self, actor: &str, action: GameAction, now: i64) -> Result<(), GameError>;
    /// Timer tick: advance the current timed phase once its deadline passes.
    fn advance(&mut self, now: i64) -> bool;
    /// What the engine owes the round — an image, and/or a text turn per bot.
    fn pending_work(&self) -> Vec<AiWork>;
    fn apply_text(
        &mut self,
        _round: u32,
        _player_id: &str,
        _kind: TextKind,
        _text: String,
        _now: i64,
    ) {
    }
    fn apply_image(&mut self, _round: u32, _art: GeneratedArt, _now: i64) {}
    /// A human-uploaded drawing snapshot (Human Pictionary). Rejected unless the
    /// caller is the current drawer and the round is live. `round`/`seq` (both
    /// optional) let the engine drop stale or out-of-order uploads.
    fn apply_drawing(
        &mut self,
        _drawer_id: &str,
        _png: Vec<u8>,
        _round: Option<u32>,
        _seq: Option<u32>,
        _now: i64,
    ) -> Result<(), GameError> {
        Err(GameError::WrongPhase)
    }
    /// The private word shortlist offered to the drawer while choosing (Human
    /// Pictionary). Empty for everyone else and in every other phase.
    fn choices_for(&self, _player_id: &str) -> Vec<String> {
        Vec::new()
    }
    /// The AI's guess at the current drawing (Human Pictionary).
    fn apply_guess(&mut self, _round: u32, _player_id: &str, _text: String, _now: i64) {}
    /// The current round's secret, delivered privately to that player only (the
    /// drawer in Human Pictionary) — never placed in the broadcast view.
    fn word_for(&self, _player_id: &str) -> Option<&str> {
        None
    }
    /// Filler for a bot whose answer turn produced nothing, so it's never
    /// obviously absent. Games override it to stay in-genre (e.g. a definition).
    fn answer_fallback(&self) -> String {
        DEFAULT_AI_FALLBACK.to_string()
    }
    /// The pending work for `round` failed (e.g. image generation unavailable).
    fn fail_work(&mut self, _round: u32, _now: i64) {}
    /// Drain the just-revealed round's human↔bot outcomes for persistence
    /// (empty unless a reveal scored since the last call).
    fn take_round_plays(&mut self) -> Vec<PlayRecord> {
        Vec::new()
    }
    /// Hand back the refreshed rivalry tallies so the view can show them.
    fn set_rivalries(&mut self, _rivalries: Rivalries) {}
    /// Re-check completion-based transitions after the roster changes — a
    /// disconnect can unblock a phase waiting on "all humans". Default: no-op.
    fn nudge(&mut self, _now: i64) {}
    fn art(&self) -> Option<RoundArt<'_>> {
        None
    }
    fn view(&self) -> RoomView;
    fn summary(&self) -> RoomSummary;
    fn is_finished(&self) -> bool;
    /// Connected humans, for room reaping. Seated bots are always "connected",
    /// so this must exclude them or an abandoned in-progress room never reaps.
    fn connected_humans(&self) -> usize;
}

const AVATARS: &[&str] = &[
    "🦊", "🐼", "🐸", "🦄", "🐙", "🐯", "🦁", "🐵", "🐧", "🦉", "🐢", "🦔", "🐝", "🦋", "🐬", "🦜",
];

/// One player's per-round anonymized seat (see [`Roster::round_seats`]).
pub struct RoundSeat {
    pub real_id: PlayerId,
    pub token: String,
}

/// Shared player management: join/reconnect, host, deterministic anon labels + avatars.
#[derive(Debug)]
pub struct Roster {
    pub room_id: String,
    /// Per-room secret salt for the deterministic anon-label shuffle and the
    /// opaque vote-pool ids. Generated server-side and NEVER serialized or
    /// broadcast (Roster is Debug-only; the `*View` structs are built field by
    /// field) — so clients can't reproduce the orderings from the public
    /// `room_id`/`round` (audit G1).
    pub salt: String,
    pub host_id: PlayerId,
    pub players: Vec<Player>,
}

impl Roster {
    pub fn new(room_id: String, host: &PlayerIdentity) -> Self {
        // Per-room secret salt (audit G1): OS-seeded via std `RandomState` —
        // each instance is independently keyed, so the value is unpredictable
        // to a remote client and is not derived from the public room_id.
        use std::hash::BuildHasher;
        let salt = format!(
            "{:016x}{:016x}",
            std::collections::hash_map::RandomState::new().hash_one(room_id.as_str()),
            std::collections::hash_map::RandomState::new().hash_one(host.id.as_str()),
        );
        Self {
            host_id: host.id.clone(),
            salt,
            players: vec![Player {
                id: host.id.clone(),
                display_name: host.display_name.clone(),
                label: String::new(),
                avatar: String::new(),
                kind: PlayerKind::Human,
                connected: false,
                score: 0,
            }],
            room_id,
        }
    }

    pub fn join(
        &mut self,
        identity: &PlayerIdentity,
        lobby_open: bool,
    ) -> Result<PlayerId, GameError> {
        if let Some(player) = self.find_mut(&identity.id) {
            player.connected = true;
            return Ok(identity.id.clone());
        }
        if !lobby_open {
            return Err(GameError::GameInProgress);
        }
        self.players.push(Player {
            id: identity.id.clone(),
            display_name: identity.display_name.clone(),
            label: String::new(),
            avatar: String::new(),
            kind: PlayerKind::Human,
            connected: true,
            score: 0,
        });
        Ok(identity.id.clone())
    }

    pub fn set_connected(&mut self, id: &str, connected: bool) {
        if let Some(player) = self.find_mut(id) {
            player.connected = connected;
        }
    }

    pub fn find(&self, id: &str) -> Option<&Player> {
        self.players.iter().find(|p| p.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Player> {
        self.players.iter_mut().find(|p| p.id == id)
    }

    pub fn humans(&self) -> impl Iterator<Item = &Player> {
        self.players.iter().filter(|p| !p.is_ai())
    }

    pub fn host_name(&self) -> String {
        self.find(&self.host_id)
            .map(|p| p.display_name.clone())
            .unwrap_or_default()
    }

    pub fn connected_humans(&self) -> usize {
        self.humans().filter(|p| p.connected).count()
    }

    /// Seat the AI as a player (Spot the Bot). Returns the synthetic player id.
    pub fn seat_ai(&mut self, agent_id: String, persona: String) -> PlayerId {
        // audit G1: no `ai-` prefix — it was a dead giveaway of the bot seat in
        // the anonymized roster. Salt the id so it isn't reproducible either.
        let id = token(&self.salt, &agent_id);
        self.players.push(Player {
            id: id.clone(),
            display_name: persona.clone(),
            label: String::new(),
            avatar: String::new(),
            kind: PlayerKind::Ai { agent_id, persona },
            connected: true,
            score: 0,
        });
        id
    }

    /// Per-round anonymized seating: a deterministic shuffle keyed on the per-room
    /// secret AND the round, giving each player a fresh opaque vote-pool `token`
    /// and a fresh order that DON'T correlate across rounds — so a prior reveal
    /// can't unmask the next round's pool (mirrors the Balderdash / Caption Battle
    /// per-round option ids). Keys are domain-separated (`o:` order, `t:` token) so
    /// neither can be derived from the other.
    pub fn round_seats(&self, round: u32) -> Vec<RoundSeat> {
        let salt = format!("{}:{round}", self.salt);
        let mut ordered: Vec<&Player> = self.players.iter().collect();
        ordered.sort_by(|a, b| {
            order_key(&salt, &format!("o:{}", a.id))
                .cmp(&order_key(&salt, &format!("o:{}", b.id)))
                .then_with(|| a.id.cmp(&b.id))
        });
        ordered
            .into_iter()
            .map(|p| RoundSeat {
                real_id: p.id.clone(),
                token: format!("{:016x}", order_key(&salt, &format!("t:{}", p.id))),
            })
            .collect()
    }

    /// Give just avatars (no anon labels) — for games that show real names.
    pub fn assign_avatars(&mut self) {
        let mut order: Vec<usize> = (0..self.players.len()).collect();
        order.sort_by_key(|&i| order_key(&self.salt, &self.players[i].id));
        for (slot, &i) in order.iter().enumerate() {
            self.players[i].avatar = AVATARS[slot % AVATARS.len()].to_string();
        }
    }

    pub fn ordered(&self) -> Vec<&Player> {
        let mut ps: Vec<&Player> = self.players.iter().collect();
        ps.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
        ps
    }

    /// Roster as anonymous player tiles (uses the assigned label/avatar).
    pub fn anon_views(&self) -> Vec<PlayerView> {
        self.ordered()
            .iter()
            .map(|p| PlayerView {
                id: p.id.clone(),
                label: p.label.clone(),
                avatar: p.avatar.clone(),
                connected: p.connected,
            })
            .collect()
    }

    /// Roster as named tiles (real display name) — for non-deduction games.
    pub fn named_views(&self) -> Vec<PlayerView> {
        self.humans()
            .map(|p| PlayerView {
                id: p.id.clone(),
                label: p.display_name.clone(),
                avatar: if p.avatar.is_empty() {
                    "🙂".into()
                } else {
                    p.avatar.clone()
                },
                connected: p.connected,
            })
            .collect()
    }
}

/// Sorted scoreboard rows, populated only once identities are revealed.
pub fn scoreboard(roster: &Roster, revealing: bool) -> Vec<ScoreRow> {
    if !revealing {
        return Vec::new();
    }
    let mut rows: Vec<ScoreRow> = roster
        .players
        .iter()
        .map(|p| ScoreRow {
            player_id: p.id.clone(),
            label: p.label.clone(),
            display_name: p.display_name.clone(),
            avatar: p.avatar.clone(),
            is_ai: p.is_ai(),
            score: p.score,
        })
        .collect();
    rows.sort_by(|a, b| b.score.cmp(&a.score).then(a.label.cmp(&b.label)));
    rows
}

pub fn order_key(room_id: &str, player_id: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    room_id.hash(&mut hasher);
    player_id.hash(&mut hasher);
    hasher.finish()
}

/// Lowercased, alphanumeric-only form for forgiving text comparison.
pub(crate) fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(squash)
        .collect()
}

/// A forgiving guess match: the whole answer (ignoring spacing/punctuation), or
/// the secret's words as a contiguous run inside the guess — so "it's a night
/// owl!" matches "night owl" and "icecream" matches "ice cream", but "category"
/// does not match "cat".
pub(crate) fn is_correct(guess: &str, secret: &str) -> bool {
    let secret_words = words(secret);
    if secret_words.is_empty() {
        return false;
    }
    squash(guess) == squash(secret)
        || words(guess)
            .windows(secret_words.len())
            .any(|window| window == secret_words.as_slice())
}

fn token(room_id: &str, agent_id: &str) -> String {
    format!("{:016x}", order_key(room_id, agent_id))
}

#[cfg(test)]
mod g1_tests {
    use super::*;
    use crate::driver::PlayerIdentity;

    #[test]
    fn vote_pool_ids_not_reproducible_from_public_inputs() {
        // audit G1: two rooms sharing the same public room_id get independent
        // secret salts, so the opaque orderings differ — a client knowing only
        // room_id/round cannot reproduce the real-answer / author ids.
        let host = PlayerIdentity {
            id: "host".into(),
            display_name: "Host".into(),
        };
        let r1 = Roster::new("ROOM".into(), &host);
        let r2 = Roster::new("ROOM".into(), &host);
        assert_ne!(r1.salt, r2.salt, "per-room salt must differ");
        assert_ne!(
            order_key(&r1.salt, "r:real"),
            order_key(&r2.salt, "r:real"),
            "opaque ids must not be reproducible across rooms"
        );
        assert_ne!(r1.salt, "ROOM", "salt must not be the public room_id");
    }
}
