use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::game::{Difficulty, Phase, PlayerId};

/// A human's running record against the bots, keyed in [`Rivalries`] by that
/// human's stable id. Personas only — never which anonymous seat is which bot.
pub type Rivalries = HashMap<String, Vec<RivalryRow>>;

/// Which game a room is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameKind {
    #[default]
    SpotTheBot,
    ReversePictionary,
    Balderdash,
    CaptionBattle,
    HumanPictionary,
}

/// A Balderdash content pack — same bluff engine, different subject matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BalderdashPack {
    #[default]
    Words,
    Movies,
    Acronyms,
    Laws,
}

/// Messages a browser sends over the room websocket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientGameMessage {
    Start,
    Submit {
        text: String,
    },
    Vote {
        target: PlayerId,
    },
    Guess {
        text: String,
    },
    /// The drawer commits to one of their offered words (Human Pictionary).
    ChooseWord {
        word: String,
    },
    Continue,
    /// The drawer asks for its private round word/shortlist (replied to that
    /// socket only).
    RequestWord,
    Ping,
}

/// Messages the server pushes to a room's browsers. `State` is a complete,
/// reconnect-safe snapshot — clients render entirely from the latest one.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerGameMessage {
    Welcome {
        player_id: PlayerId,
    },
    State(Box<RoomView>),
    Error {
        message: String,
    },
    /// The drawer's private secret word (never broadcast).
    YourWord {
        word: String,
    },
    /// The drawer's private word shortlist while choosing (never broadcast).
    WordChoices {
        words: Vec<String>,
    },
    Pong,
}

impl ServerGameMessage {
    pub fn state(view: RoomView) -> Self {
        Self::State(Box::new(view))
    }
}

/// The full render state of a room: a shared envelope plus a game-specific payload.
#[derive(Debug, Clone, Serialize)]
pub struct RoomView {
    pub room_id: String,
    pub kind: GameKind,
    pub phase: Phase,
    pub round: u32,
    pub total_rounds: u32,
    pub difficulty: Difficulty,
    pub host_id: PlayerId,
    pub players: Vec<PlayerView>,
    pub ends_at: Option<i64>,
    pub scoreboard: Vec<ScoreRow>,
    pub game: GameView,
}

/// Game-specific render payload, tagged by `view`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum GameView {
    SpotTheBot {
        prompt: Option<String>,
        submitted_count: u32,
        player_count: u32,
        bots_writing: bool,
        responses: Vec<AnonResponse>,
        reveal: Option<RevealView>,
        rivalries: Rivalries,
    },
    ReversePictionary {
        artist: String,
        art_round: Option<u32>,
        guessed: Vec<GuessRow>,
        secret: Option<String>,
        unavailable: bool,
    },
    Balderdash {
        word: String,
        /// What players are bluffing ("definition", "plot", "expansion", "law").
        noun: String,
        submitted_count: u32,
        player_count: u32,
        bots_writing: bool,
        options: Vec<DefOptionView>,
        reveal: Option<BalderdashReveal>,
        rivalries: Rivalries,
    },
    CaptionBattle {
        prompt: String,
        submitted_count: u32,
        player_count: u32,
        bots_writing: bool,
        options: Vec<DefOptionView>,
        reveal: Option<CaptionReveal>,
    },
    HumanPictionary {
        drawer_id: Option<PlayerId>,
        drawer_label: Option<String>,
        /// Set once a snapshot exists; bumps each upload so watchers re-fetch /art.
        art_round: Option<u32>,
        snapshot: u32,
        /// The round chat — guesses and banter alike (the secret is never echoed).
        guesses: Vec<GuessFeedRow>,
        /// Everyone who has guessed the word this round, in order, with the points
        /// they earned. Authoritative for "have I solved it" (the chat feed churns).
        correct: Vec<RoundGuessRow>,
        /// What the drawer has earned this round (+2 per correct guesser).
        drawer_points: u32,
        /// The secret word — only when revealing; never sent to guessers mid-round.
        secret: Option<String>,
        unavailable: bool,
    },
}

/// One solver in a Human Pictionary round — shown at reveal with their points.
#[derive(Debug, Clone, Serialize)]
pub struct RoundGuessRow {
    pub player_id: PlayerId,
    pub label: String,
    pub avatar: String,
    pub is_ai: bool,
    pub points: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuessFeedRow {
    pub player_id: PlayerId,
    pub label: String,
    pub avatar: String,
    pub text: String,
    pub correct: bool,
    pub is_ai: bool,
    /// The author already knows the word (the drawer, or a solver chatting on) —
    /// the UI tags these so guessers can tell banter from a fellow guesser's try.
    pub solved: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptionReveal {
    pub entries: Vec<RevealedCaption>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevealedCaption {
    pub text: String,
    pub author_display_name: Option<String>,
    pub is_ai: bool,
    pub persona: Option<String>,
    pub votes_received: u32,
    pub is_winner: bool,
    pub jab: Option<String>,
}

/// One anonymized definition to vote on. `id` is opaque + uniform across every
/// option so the real one can't be told apart, and authorship never leaks.
#[derive(Debug, Clone, Serialize)]
pub struct DefOptionView {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalderdashReveal {
    pub real_text: String,
    pub entries: Vec<RevealedDef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevealedDef {
    pub text: String,
    pub is_real: bool,
    pub author_display_name: Option<String>,
    pub is_ai: bool,
    pub persona: Option<String>,
    pub votes_received: u32,
    pub jab: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerView {
    pub id: PlayerId,
    pub label: String,
    pub avatar: String,
    pub connected: bool,
}

/// One anonymized answer to vote on. `player_id` is the author's opaque per-round
/// token (uniform across the pool); no label or avatar is shown, so nothing about
/// a tile correlates with a prior round's reveal (mirrors Balderdash's options).
#[derive(Debug, Clone, Serialize)]
pub struct AnonResponse {
    pub player_id: PlayerId,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevealView {
    pub players: Vec<RevealedPlayer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevealedPlayer {
    pub id: PlayerId,
    pub display_name: String,
    /// The answer this player gave, unmasked — what ties each vote tile to its author.
    pub answer: Option<String>,
    pub is_ai: bool,
    pub persona: Option<String>,
    pub votes_received: u32,
    /// The bot's one-line, in-character reaction to the round (AI seats only).
    pub jab: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RivalryRow {
    pub persona: String,
    pub caught: u32,
    pub fooled: u32,
    pub plays: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuessRow {
    pub player_id: PlayerId,
    pub label: String,
    pub avatar: String,
    pub rank: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoreRow {
    pub player_id: PlayerId,
    pub label: String,
    pub display_name: String,
    pub avatar: String,
    pub is_ai: bool,
    pub score: u32,
}

/// Lobby listing entry.
#[derive(Debug, Clone, Serialize)]
pub struct RoomSummary {
    pub room_id: String,
    pub kind: GameKind,
    pub phase: Phase,
    pub player_count: u32,
    pub host: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateRoomRequest {
    #[serde(default)]
    pub kind: GameKind,
    pub rounds: Option<u32>,
    /// Number of AI players to seat (Spot the Bot); clamped to the configured agents.
    pub bots: Option<u8>,
    pub difficulty: Option<Difficulty>,
    /// Content pack (Balderdash only).
    pub pack: Option<BalderdashPack>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GamesConfigView {
    pub max_bots: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRoomResponse {
    pub room_id: String,
}
