use std::collections::HashMap;

use crate::driver::{PlayRecord, PlayerIdentity};
use crate::game::{
    order_key, scoreboard, squash, AiSeat, AiWork, Difficulty, Game, GameAction, GameError, Phase,
    PlayerId, PlayerKind, RoomConfig, Roster, TextKind,
};
use crate::protocol::{
    BalderdashPack, BalderdashReveal, DefOptionView, GameKind, GameView, RevealedDef, Rivalries,
    RoomSummary, RoomView,
};

// Word banks: (obscure real word, its real definition). Obscurity rises with
// difficulty; the definitions are genuine, since players vote for the real one.
const EASY: &[(&str, &str)] = &[
    (
        "petrichor",
        "the pleasant earthy smell after rain falls on dry ground",
    ),
    (
        "defenestrate",
        "to throw someone or something out of a window",
    ),
    (
        "gerrymander",
        "to manipulate the boundaries of an electoral district to favour one party",
    ),
    ("ineffable", "too great or extreme to be expressed in words"),
    ("flummox", "to bewilder or confuse someone completely"),
    (
        "kerfuffle",
        "a commotion or fuss, especially one caused by conflicting views",
    ),
    ("lollygag", "to spend time aimlessly; to dawdle"),
    ("brouhaha", "a noisy and overexcited reaction to something"),
    (
        "collywobbles",
        "stomach pain or queasiness, or nervous butterflies",
    ),
    (
        "widdershins",
        "in a direction contrary to the sun's course; counterclockwise",
    ),
    (
        "gobbledygook",
        "language that is meaningless or hard to understand; jargon",
    ),
];

const NORMAL: &[(&str, &str)] = &[
    (
        "borborygmus",
        "a rumbling noise made by the movement of gas in the intestines",
    ),
    ("absquatulate", "to leave abruptly or abscond"),
    ("cattywampus", "askew, awry, or positioned diagonally"),
    (
        "mumpsimus",
        "a person who obstinately clings to a mistaken belief or practice",
    ),
    (
        "pandiculation",
        "the act of stretching and yawning, especially on waking",
    ),
    (
        "ultracrepidarian",
        "a person who gives opinions on matters beyond their knowledge",
    ),
    (
        "zugzwang",
        "a situation in which any move a player makes worsens their position",
    ),
    ("yclept", "by the name of; called"),
    ("sialoquent", "tending to spray saliva while speaking"),
    (
        "nudiustertian",
        "of or relating to the day before yesterday",
    ),
    (
        "snollygoster",
        "a shrewd, unprincipled person, especially a politician",
    ),
];

const HARD: &[(&str, &str)] = &[
    (
        "velleity",
        "a wish or inclination too slight to lead to action",
    ),
    ("gormless", "lacking sense or initiative; foolish"),
    ("sprezzatura", "studied carelessness; effortless grace"),
    (
        "limerence",
        "the state of being infatuated or obsessed with another person",
    ),
    ("apricity", "the warmth of the sun in winter"),
    (
        "grandiloquent",
        "pompous or extravagant in language or style",
    ),
    (
        "hebetude",
        "the state of being dull, lethargic, or slow-witted",
    ),
    (
        "nepenthe",
        "a drug or drink that brings forgetfulness of sorrow",
    ),
    (
        "recrudescence",
        "the recurrence of an undesirable condition after a period of abatement",
    ),
    (
        "eucatastrophe",
        "a sudden and favourable resolution of events in a story",
    ),
    (
        "quockerwodger",
        "a wooden puppet, or a politician whose strings are pulled by another",
    ),
];

const VERY_HARD: &[(&str, &str)] = &[
    (
        "floccinaucinihilipilification",
        "the act of estimating something as worthless",
    ),
    (
        "hugger-mugger",
        "secrecy or concealment, or a state of confusion and muddle",
    ),
    ("welkin", "the sky or the vault of heaven"),
    ("vagitus", "the cry of a newborn child"),
    ("zenzizenzizenzic", "a number raised to the eighth power"),
    (
        "gardyloo",
        "a warning cry once shouted before throwing slops from a window",
    ),
    ("jentacular", "of or relating to breakfast"),
    (
        "mallemaroking",
        "the carousing of seamen aboard ice-bound ships",
    ),
    ("pannychis", "an all-night feast or vigil"),
    ("quomodocunquize", "to make money by any means possible"),
];

// Content packs (same engine, different subject matter). Each is a bank of real
// (subject, real answer) pairs; players bluff the answer. Difficulty changes the
// Words bank's obscurity and, for every pack, how convincing the bot's bluff is.
const MOVIES: &[(&str, &str)] = &[
    (
        "Primer",
        "two engineers accidentally build a time-travel device in their garage",
    ),
    (
        "Coherence",
        "a dinner party fractures into parallel versions of itself as a comet passes",
    ),
    (
        "The Lobster",
        "single people must find a partner in 45 days or be turned into an animal",
    ),
    (
        "Swiss Army Man",
        "a stranded man befriends a multipurpose talking corpse to get home",
    ),
    (
        "Sorry to Bother You",
        "a telemarketer's 'white voice' pulls him into a surreal corporate plot",
    ),
    (
        "The Fall",
        "a hospitalized stuntman spins an epic fantasy tale for a young girl",
    ),
    (
        "Dogtooth",
        "parents raise their children in isolation with a deliberately warped vocabulary",
    ),
    (
        "Upstream Color",
        "two strangers are unknowingly linked by a parasite's strange life cycle",
    ),
    (
        "Tetsuo: The Iron Man",
        "a man slowly and grotesquely mutates into a mass of scrap metal",
    ),
    (
        "Being John Malkovich",
        "office workers find a portal into the head of actor John Malkovich",
    ),
];

const ACRONYMS: &[(&str, &str)] = &[
    ("SCUBA", "self-contained underwater breathing apparatus"),
    (
        "LASER",
        "light amplification by stimulated emission of radiation",
    ),
    ("TASER", "Thomas A. Swift's electric rifle"),
    (
        "CAPTCHA",
        "completely automated public Turing test to tell computers and humans apart",
    ),
    ("BASIC", "beginner's all-purpose symbolic instruction code"),
    ("NABISCO", "National Biscuit Company"),
    ("ZIP (code)", "zone improvement plan"),
    (
        "GULAG",
        "the Soviet agency that ran the forced-labour camps",
    ),
    (
        "SPECTRE",
        "Special Executive for Counter-intelligence, Terrorism, Revenge and Extortion",
    ),
    ("SNAFU", "situation normal: all fouled up"),
];

const LAWS: &[(&str, &str)] = &[
    (
        "In Switzerland, it is illegal to…",
        "keep just one guinea pig, because they get lonely",
    ),
    (
        "In Singapore, it is illegal to…",
        "import or sell chewing gum",
    ),
    (
        "In the UK, it is an offence to…",
        "handle a salmon in suspicious circumstances",
    ),
    (
        "On Germany's Autobahn, it is illegal to…",
        "stop or run out of fuel for no good reason",
    ),
    (
        "In Denmark, it is not a punishable crime to…",
        "escape from prison",
    ),
    (
        "In Thailand, it is illegal to…",
        "step on money, because it bears the king's image",
    ),
    (
        "In Venice, Italy, it is illegal to…",
        "feed the pigeons in St Mark's Square",
    ),
    (
        "In Japan until 2015, it was illegal to…",
        "dance in clubs after midnight",
    ),
];

fn word_bank(difficulty: Difficulty) -> &'static [(&'static str, &'static str)] {
    match difficulty {
        Difficulty::Easy => EASY,
        Difficulty::Normal => NORMAL,
        Difficulty::Hard => HARD,
        Difficulty::VeryHard => VERY_HARD,
    }
}

fn pack_bank(
    pack: BalderdashPack,
    difficulty: Difficulty,
) -> &'static [(&'static str, &'static str)] {
    match pack {
        BalderdashPack::Words => word_bank(difficulty),
        BalderdashPack::Movies => MOVIES,
        BalderdashPack::Acronyms => ACRONYMS,
        BalderdashPack::Laws => LAWS,
    }
}

/// What players are bluffing, for the UI ("Bluff a {noun}").
fn pack_noun(pack: BalderdashPack) -> &'static str {
    match pack {
        BalderdashPack::Words => "definition",
        BalderdashPack::Movies => "plot",
        BalderdashPack::Acronyms => "expansion",
        BalderdashPack::Laws => "law",
    }
}

/// A poor-but-safe definition if a bot's turn fails — shared `AI_FALLBACK` is
/// not definition-shaped.
const BLUFF_FALLBACK: &str = "of uncertain or disputed origin; rarely used in modern speech";

/// Who wrote a definition — kept server-side; never sent to clients during voting.
enum DefSource {
    Author(PlayerId),
    Real,
}

/// A definition in the round's vote pool. `id` is an opaque, per-round, uniform
/// hash so the real one and each author stay hidden until the reveal.
struct DefOption {
    id: String,
    source: DefSource,
    text: String,
}

/// Balderdash: every player (humans + AI bots) writes a fake definition for an
/// obscure word; the real definition is hidden among them; humans vote for the
/// one they think is real. Score for finding it and for each player you fool.
pub struct Balderdash {
    roster: Roster,
    config: RoomConfig,
    pack: BalderdashPack,
    bank: &'static [(&'static str, &'static str)],
    seats: Vec<AiSeat>,
    bot_ids: Vec<PlayerId>,
    phase: Phase,
    round: u32,
    deadline: i64,
    word_offset: usize,
    word: Option<&'static str>,
    real_def: Option<&'static str>,
    submissions: HashMap<PlayerId, String>,
    options: Vec<DefOption>,
    votes: HashMap<PlayerId, String>,
    jabs: HashMap<PlayerId, String>,
    round_plays: Vec<PlayRecord>,
    rivalries: Rivalries,
}

impl Balderdash {
    pub fn new(
        room_id: String,
        host: &PlayerIdentity,
        config: RoomConfig,
        seats: Vec<AiSeat>,
        pack: BalderdashPack,
    ) -> Self {
        let bank = pack_bank(pack, config.difficulty);
        let word_offset = (order_key(&room_id, "") % bank.len() as u64) as usize;
        Self {
            roster: Roster::new(room_id, host),
            config,
            pack,
            bank,
            seats,
            bot_ids: Vec::new(),
            phase: Phase::Lobby,
            round: 0,
            deadline: 0,
            word_offset,
            word: None,
            real_def: None,
            submissions: HashMap::new(),
            options: Vec::new(),
            votes: HashMap::new(),
            jabs: HashMap::new(),
            round_plays: Vec::new(),
            rivalries: Rivalries::new(),
        }
    }

    fn start(&mut self, actor: &str, now: i64) -> Result<(), GameError> {
        if actor != self.roster.host_id {
            return Err(GameError::NotHost);
        }
        if self.phase != Phase::Lobby {
            return Err(GameError::NotLobby);
        }
        if self.roster.connected_humans() < self.config.min_humans {
            return Err(GameError::NeedMoreHumans(self.config.min_humans));
        }
        if self.seats.is_empty() {
            return Err(GameError::NoAiAvailable);
        }
        self.bot_ids = self
            .seats
            .clone()
            .into_iter()
            .map(|seat| self.roster.seat_ai(seat.agent_id, seat.persona))
            .collect();
        self.roster.assign_avatars();
        self.begin_round(0, now);
        Ok(())
    }

    fn submit(&mut self, actor: &str, text: String, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Collecting {
            return Err(GameError::WrongPhase);
        }
        match self.roster.find(actor) {
            Some(p) if p.is_ai() => return Err(GameError::NotAPlayer),
            None => return Err(GameError::NotAPlayer),
            _ => {}
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.submissions.insert(actor.to_string(), text);
        if self.all_submitted() {
            self.begin_voting(now);
        }
        Ok(())
    }

    fn vote(&mut self, actor: &str, option_id: &str, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Voting {
            return Err(GameError::WrongPhase);
        }
        match self.roster.find(actor) {
            Some(p) if p.is_ai() => return Err(GameError::NotAPlayer),
            None => return Err(GameError::NotAPlayer),
            _ => {}
        }
        let Some(option) = self.options.iter().find(|o| o.id == option_id) else {
            return Err(GameError::UnknownTarget);
        };
        if matches!(&option.source, DefSource::Author(a) if a == actor) {
            return Err(GameError::SelfVote);
        }
        self.votes.insert(actor.to_string(), option_id.to_string());
        if self.all_voted() {
            self.begin_reveal(now);
        }
        Ok(())
    }

    fn begin_round(&mut self, round: u32, now: i64) {
        self.round = round;
        self.phase = Phase::Collecting;
        let (word, def) = self.bank[(self.word_offset + round as usize) % self.bank.len()];
        self.word = Some(word);
        self.real_def = Some(def);
        self.submissions.clear();
        self.options.clear();
        self.votes.clear();
        self.jabs.clear();
        self.deadline = now + self.config.collect_secs;
    }

    fn begin_voting(&mut self, now: i64) {
        self.options = self.build_options();
        self.phase = Phase::Voting;
        self.deadline = now + self.config.vote_secs;
    }

    fn begin_reveal(&mut self, now: i64) {
        self.score_round();
        self.round_plays = self.build_round_plays();
        self.phase = Phase::Reveal;
        self.deadline = now + self.config.reveal_secs;
    }

    /// On to the next round, or end the game. Shared by `advance` and Continue.
    fn end_reveal(&mut self, now: i64) {
        let next = self.round + 1;
        if next < self.config.total_rounds {
            self.begin_round(next, now);
        } else {
            self.phase = Phase::GameOver;
            self.deadline = 0;
        }
    }

    fn host_continue(&mut self, actor: &str, now: i64) -> Result<(), GameError> {
        if actor != self.roster.host_id {
            return Err(GameError::NotHost);
        }
        if self.phase != Phase::Reveal {
            return Err(GameError::WrongPhase);
        }
        self.end_reveal(now);
        Ok(())
    }

    /// The shuffled vote pool: each bluff plus the real definition, behind opaque
    /// per-round ids so neither authorship nor which-is-real leaks. A bot bluff
    /// that lands on the real definition is dropped (no duplicate option).
    fn build_options(&self) -> Vec<DefOption> {
        // audit G1: salt with the per-room SECRET (not the public room_id) so a
        // client can't compute the real-definition / author ids and vote them.
        let salt = format!("{}:{}", self.roster.salt, self.round);
        let real_squashed = self.real_def.map(squash).unwrap_or_default();
        // Keys are domain-separated (`p:` authors, `r:` real) so no key content
        // can collide regardless of player-id shape, and salted by round so an
        // option's position never correlates across rounds.
        let mut keyed: Vec<(u64, DefSource, String)> = Vec::new();
        for (author, text) in &self.submissions {
            if self.bot_ids.iter().any(|b| b == author) && squash(text) == real_squashed {
                continue;
            }
            keyed.push((
                order_key(&salt, &format!("p:{author}")),
                DefSource::Author(author.clone()),
                text.clone(),
            ));
        }
        if let Some(def) = self.real_def {
            keyed.push((order_key(&salt, "r:real"), DefSource::Real, def.to_string()));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        keyed
            .into_iter()
            .map(|(key, source, text)| DefOption {
                id: format!("{key:016x}"),
                source,
                text,
            })
            .collect()
    }

    fn score_round(&mut self) {
        let mut awards: Vec<(PlayerId, u32)> = Vec::new();
        for (voter, option_id) in &self.votes {
            if let Some(option) = self.options.iter().find(|o| &o.id == option_id) {
                match &option.source {
                    DefSource::Real => awards.push((voter.clone(), 2)),
                    DefSource::Author(author) => awards.push((author.clone(), 1)),
                }
            }
        }
        for (id, points) in awards {
            if let Some(player) = self.roster.find_mut(&id) {
                player.score += points;
            }
        }
    }

    /// One record per (human voter × bot): `caught` = that human did NOT fall for
    /// the bot's bluff this round. Mirrors Spot the Bot's "saw through the AI".
    fn build_round_plays(&self) -> Vec<PlayRecord> {
        let mut plays = Vec::new();
        for (voter, option_id) in &self.votes {
            for bot_id in &self.bot_ids {
                let Some(bot_option) = self.option_for(bot_id) else {
                    continue;
                };
                let Some(agent_id) = self.agent_id(bot_id) else {
                    continue;
                };
                plays.push(PlayRecord {
                    human_id: voter.clone(),
                    agent_id,
                    caught: option_id != &bot_option.id,
                });
            }
        }
        plays
    }

    fn all_submitted(&self) -> bool {
        self.roster
            .humans()
            .filter(|p| p.connected)
            .all(|p| self.submissions.contains_key(&p.id))
            && self
                .bot_ids
                .iter()
                .all(|id| self.submissions.contains_key(id))
    }

    fn all_bots_answered(&self) -> bool {
        self.bot_ids
            .iter()
            .all(|id| self.submissions.contains_key(id))
    }

    fn all_voted(&self) -> bool {
        self.roster
            .humans()
            .filter(|p| p.connected)
            .all(|p| self.votes.contains_key(&p.id))
    }

    fn bots_writing(&self) -> bool {
        self.phase == Phase::Collecting && !self.all_bots_answered()
    }

    fn option_for(&self, author: &str) -> Option<&DefOption> {
        self.options
            .iter()
            .find(|o| matches!(&o.source, DefSource::Author(a) if a == author))
    }

    fn votes_for(&self, option_id: &str) -> u32 {
        self.votes
            .values()
            .filter(|v| v.as_str() == option_id)
            .count() as u32
    }

    fn agent_id(&self, bot_id: &str) -> Option<String> {
        self.roster.players.iter().find_map(|p| match &p.kind {
            PlayerKind::Ai { agent_id, .. } if p.id.as_str() == bot_id => Some(agent_id.clone()),
            _ => None,
        })
    }

    fn build_reveal(&self) -> BalderdashReveal {
        let entries = self
            .options
            .iter()
            .map(|option| {
                let votes_received = self.votes_for(&option.id);
                match &option.source {
                    DefSource::Real => RevealedDef {
                        text: option.text.clone(),
                        is_real: true,
                        author_display_name: None,
                        is_ai: false,
                        persona: None,
                        votes_received,
                        jab: None,
                    },
                    DefSource::Author(author) => {
                        let player = self.roster.find(author);
                        RevealedDef {
                            text: option.text.clone(),
                            is_real: false,
                            author_display_name: player.map(|p| p.display_name.clone()),
                            is_ai: player.map(|p| p.is_ai()).unwrap_or(false),
                            persona: player.and_then(|p| p.persona().map(str::to_string)),
                            votes_received,
                            jab: self.jabs.get(author).cloned(),
                        }
                    }
                }
            })
            .collect();
        BalderdashReveal {
            real_text: self.real_def.unwrap_or_default().to_string(),
            entries,
        }
    }
}

impl Game for Balderdash {
    fn join(&mut self, identity: &PlayerIdentity) -> Result<PlayerId, GameError> {
        self.roster.join(identity, self.phase == Phase::Lobby)
    }

    fn set_connected(&mut self, id: &str, connected: bool) {
        self.roster.set_connected(id, connected);
    }

    fn act(&mut self, actor: &str, action: GameAction, now: i64) -> Result<(), GameError> {
        match action {
            GameAction::Start => self.start(actor, now),
            GameAction::Submit(text) => self.submit(actor, text, now),
            GameAction::Vote(option_id) => self.vote(actor, &option_id, now),
            GameAction::Continue => self.host_continue(actor, now),
            GameAction::Guess(_) | GameAction::Choose(_) => Err(GameError::WrongPhase),
        }
    }

    fn advance(&mut self, now: i64) -> bool {
        if !matches!(
            self.phase,
            Phase::Collecting | Phase::Voting | Phase::Reveal
        ) || now < self.deadline
        {
            return false;
        }
        match self.phase {
            // Timeout: vote over whatever was submitted plus the real definition.
            Phase::Collecting => self.begin_voting(now),
            Phase::Voting => self.begin_reveal(now),
            Phase::Reveal => self.end_reveal(now),
            _ => unreachable!(),
        }
        true
    }

    fn pending_work(&self) -> Vec<AiWork> {
        match self.phase {
            Phase::Collecting => {
                let Some(word) = self.word else {
                    return Vec::new();
                };
                self.bot_ids
                    .iter()
                    .filter(|id| !self.submissions.contains_key(*id))
                    .filter_map(|id| {
                        Some(AiWork::Text {
                            round: self.round,
                            player_id: id.clone(),
                            agent_id: self.agent_id(id)?,
                            kind: TextKind::Answer,
                            prompt: bluff_prompt(self.pack, word, self.config.difficulty),
                        })
                    })
                    .collect()
            }
            Phase::Reveal => self
                .bot_ids
                .iter()
                .filter(|id| !self.jabs.contains_key(*id))
                .filter_map(|id| {
                    let option = self.option_for(id)?;
                    let fooled = self.votes_for(&option.id);
                    Some(AiWork::Text {
                        round: self.round,
                        player_id: id.clone(),
                        agent_id: self.agent_id(id)?,
                        kind: TextKind::Jab,
                        prompt: jab_prompt(self.roster.find(id)?.persona()?, fooled),
                    })
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn apply_text(&mut self, round: u32, player_id: &str, kind: TextKind, text: String, now: i64) {
        if round != self.round || !self.bot_ids.iter().any(|id| id == player_id) {
            return;
        }
        match kind {
            // The manager substitutes `answer_fallback()` for an empty reply, so
            // the bluff is always present and definition-shaped.
            TextKind::Answer => {
                if self.phase == Phase::Collecting {
                    self.submissions
                        .insert(player_id.to_string(), text.trim().to_string());
                    if self.all_submitted() {
                        self.begin_voting(now);
                    }
                }
            }
            TextKind::Jab => {
                if self.phase == Phase::Reveal && !text.trim().is_empty() {
                    self.jabs.insert(player_id.to_string(), text);
                }
            }
        }
    }

    fn answer_fallback(&self) -> String {
        BLUFF_FALLBACK.to_string()
    }

    fn take_round_plays(&mut self) -> Vec<PlayRecord> {
        std::mem::take(&mut self.round_plays)
    }

    fn set_rivalries(&mut self, rivalries: Rivalries) {
        self.rivalries = rivalries;
    }

    fn nudge(&mut self, now: i64) {
        match self.phase {
            Phase::Collecting if self.all_submitted() => self.begin_voting(now),
            Phase::Voting if self.all_voted() => self.begin_reveal(now),
            _ => {}
        }
    }

    fn view(&self) -> RoomView {
        let revealing = matches!(self.phase, Phase::Reveal | Phase::GameOver);
        let options = if self.phase == Phase::Voting {
            self.options
                .iter()
                .map(|o| DefOptionView {
                    id: o.id.clone(),
                    text: o.text.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };

        RoomView {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::Balderdash,
            phase: self.phase,
            round: self.round,
            total_rounds: self.config.total_rounds,
            difficulty: self.config.difficulty,
            host_id: self.roster.host_id.clone(),
            players: self.roster.named_views(),
            ends_at: match self.phase {
                Phase::Lobby | Phase::GameOver => None,
                _ => Some(self.deadline),
            },
            scoreboard: scoreboard(&self.roster, revealing),
            game: GameView::Balderdash {
                word: self.word.unwrap_or_default().to_string(),
                noun: pack_noun(self.pack).to_string(),
                submitted_count: self.submissions.len() as u32,
                player_count: self.roster.players.len() as u32,
                bots_writing: self.bots_writing(),
                options,
                reveal: revealing.then(|| self.build_reveal()),
                rivalries: if revealing {
                    self.rivalries.clone()
                } else {
                    Rivalries::new()
                },
            },
        }
    }

    fn summary(&self) -> RoomSummary {
        RoomSummary {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::Balderdash,
            phase: self.phase,
            player_count: self.roster.players.len() as u32,
            host: self.roster.host_name(),
        }
    }

    fn is_finished(&self) -> bool {
        self.phase == Phase::GameOver
    }

    fn connected_humans(&self) -> usize {
        self.roster.connected_humans()
    }
}

fn bluff_prompt(pack: BalderdashPack, subject: &str, difficulty: Difficulty) -> String {
    let task = match pack {
        BalderdashPack::Words => {
            format!("invent a FALSE but believable definition for the obscure word \"{subject}\"")
        }
        BalderdashPack::Movies => {
            format!("invent a FALSE but believable one-line plot for the film \"{subject}\"")
        }
        BalderdashPack::Acronyms => format!(
            "invent a FALSE but believable expansion for the acronym \"{subject}\" — what the \
             letters supposedly stand for"
        ),
        BalderdashPack::Laws => {
            format!("invent a FALSE but believable way to finish this real law: \"{subject}\"")
        }
    };
    let style = match difficulty {
        Difficulty::Easy => "Keep it short and a little vague — it doesn't need to be airtight.",
        Difficulty::Normal => "Make it sound real: plausible, confident, concise.",
        Difficulty::Hard => {
            "Make it genuinely convincing — the kind of answer people would believe over the real one."
        }
        Difficulty::VeryHard => {
            "Make it impossible to tell from the real thing: exact register, confident phrasing."
        }
    };
    format!(
        "Balderdash — {task}. Make it up; do NOT give the real answer even if you happen to know it. \
         {style} One short line. Reply with only your answer."
    )
}

/// In-character reveal reaction. Counts only — no player names/ids — so the
/// agent turn stays free of player data.
fn jab_prompt(persona: &str, fooled: u32) -> String {
    format!(
        "Balderdash reveal — you're {persona}, the AI, now unmasked. Your fake definition fooled \
         {fooled} player(s) into thinking it was the real one. Fire back ONE short, playful, \
         in-character line about your bluff — no names, just react. Reply with only the line."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> PlayerIdentity {
        PlayerIdentity {
            id: name.to_string(),
            display_name: name.to_uppercase(),
        }
    }

    fn seats(personas: &[&str]) -> Vec<AiSeat> {
        personas
            .iter()
            .map(|p| AiSeat {
                agent_id: format!("{}_game", p.to_lowercase()),
                persona: p.to_string(),
            })
            .collect()
    }

    fn lobby_with(humans: &[&str], personas: &[&str]) -> Balderdash {
        let mut game = Balderdash::new(
            "room1".into(),
            &id(humans[0]),
            RoomConfig::default(),
            seats(personas),
            BalderdashPack::Words,
        );
        for h in humans {
            game.join(&id(h)).unwrap();
        }
        game
    }

    /// Humans submit bluffs, then bots answer → the room reaches Voting.
    fn to_voting(game: &mut Balderdash, humans: &[&str]) {
        for h in humans {
            game.act(
                h,
                GameAction::Submit(format!("{h}'s plausible definition")),
                0,
            )
            .unwrap();
        }
        for (i, bot) in game.bot_ids.clone().iter().enumerate() {
            game.apply_text(
                0,
                bot,
                TextKind::Answer,
                format!("a fabricated meaning {i}"),
                0,
            );
        }
    }

    fn author_opt(game: &Balderdash, who: &str) -> String {
        game.options
            .iter()
            .find(|o| matches!(&o.source, DefSource::Author(a) if a == who))
            .unwrap()
            .id
            .clone()
    }

    fn real_opt(game: &Balderdash) -> String {
        game.options
            .iter()
            .find(|o| matches!(o.source, DefSource::Real))
            .unwrap()
            .id
            .clone()
    }

    #[test]
    fn bots_owe_a_bluff_at_round_start_without_the_real_meaning() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny", "Bernard"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        assert_eq!(game.phase, Phase::Collecting);
        let work = game.pending_work();
        assert_eq!(work.len(), 2); // both bots, in parallel with humans
        for w in &work {
            let AiWork::Text { kind, prompt, .. } = w else {
                panic!("expected text")
            };
            assert_eq!(*kind, TextKind::Answer);
            assert!(prompt.contains(game.word.unwrap()));
            assert!(!prompt.contains(game.real_def.unwrap())); // real meaning never leaked
        }
    }

    #[test]
    fn voting_pool_hides_the_real_definition() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob"]);
        assert_eq!(game.phase, Phase::Voting);
        match game.view().game {
            GameView::Balderdash {
                options, reveal, ..
            } => {
                assert!(reveal.is_none());
                assert_eq!(options.len(), 4); // alice + bob + 1 bot + real
                for o in &options {
                    assert_eq!(o.id.len(), 16);
                    assert!(o.id.chars().all(|c| c.is_ascii_hexdigit()));
                }
                assert!(options.iter().any(|o| o.text == game.real_def.unwrap()));
            }
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn vote_rejects_self_and_unknown_but_allows_real() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob"]);
        assert_eq!(
            game.act("alice", GameAction::Vote(author_opt(&game, "alice")), 0),
            Err(GameError::SelfVote)
        );
        assert_eq!(
            game.act("alice", GameAction::Vote("00000000deadbeef".into()), 0),
            Err(GameError::UnknownTarget)
        );
        game.act("alice", GameAction::Vote(real_opt(&game)), 0)
            .unwrap();
    }

    #[test]
    fn scoring_awards_finders_and_bluffers() {
        let mut game = lobby_with(&["alice", "bob", "carol"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob", "carol"]);
        let penny = game.bot_ids[0].clone();
        game.act("alice", GameAction::Vote(real_opt(&game)), 0)
            .unwrap(); // alice finds it
        game.act("bob", GameAction::Vote(author_opt(&game, &penny)), 0)
            .unwrap(); // bob fooled by bot
        game.act("carol", GameAction::Vote(author_opt(&game, "alice")), 0)
            .unwrap(); // carol fooled by alice
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(game.roster.find("alice").unwrap().score, 3); // +2 found + 1 fooled carol
        assert_eq!(game.roster.find("bob").unwrap().score, 0);
        assert_eq!(game.roster.find("carol").unwrap().score, 0);
        assert_eq!(game.roster.find(&penny).unwrap().score, 1); // bot fooled bob
    }

    #[test]
    fn bot_bluff_matching_real_def_is_dropped() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let real = game.real_def.unwrap().to_string();
        let penny = game.bot_ids[0].clone();
        game.act("alice", GameAction::Submit("a".into()), 0)
            .unwrap();
        game.act("bob", GameAction::Submit("b".into()), 0).unwrap();
        game.apply_text(0, &penny, TextKind::Answer, real.clone(), 0); // bot regurgitates the real def
        assert_eq!(game.phase, Phase::Voting);
        assert_eq!(game.options.len(), 3); // alice + bob + real (bot's duplicate dropped)
        assert_eq!(game.options.iter().filter(|o| o.text == real).count(), 1);
    }

    #[test]
    fn difficulty_selects_def_bank_deterministically() {
        for (difficulty, bank) in [
            (Difficulty::Easy, EASY),
            (Difficulty::Normal, NORMAL),
            (Difficulty::Hard, HARD),
            (Difficulty::VeryHard, VERY_HARD),
        ] {
            let cfg = RoomConfig {
                difficulty,
                ..RoomConfig::default()
            };
            let mut game = Balderdash::new(
                "room1".into(),
                &id("alice"),
                cfg,
                seats(&["Penny"]),
                BalderdashPack::Words,
            );
            game.join(&id("alice")).unwrap();
            game.act("alice", GameAction::Start, 0).unwrap();
            let (word, def) = (game.word.unwrap(), game.real_def.unwrap());
            assert!(bank.iter().any(|(w, d)| *w == word && *d == def));

            let cfg = RoomConfig {
                difficulty,
                ..RoomConfig::default()
            };
            let mut again = Balderdash::new(
                "room1".into(),
                &id("alice"),
                cfg,
                seats(&["Penny"]),
                BalderdashPack::Words,
            );
            again.join(&id("alice")).unwrap();
            again.act("alice", GameAction::Start, 0).unwrap();
            assert_eq!(again.word.unwrap(), word);
        }
    }

    #[test]
    fn host_continue_skips_reveal_and_rejects_non_host() {
        let cfg = RoomConfig {
            total_rounds: 2,
            ..RoomConfig::default()
        };
        let mut game = Balderdash::new(
            "room1".into(),
            &id("alice"),
            cfg,
            seats(&["Penny"]),
            BalderdashPack::Words,
        );
        game.join(&id("alice")).unwrap();
        game.join(&id("bob")).unwrap();
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob"]);
        let real = real_opt(&game);
        game.act("alice", GameAction::Vote(real.clone()), 0)
            .unwrap();
        game.act("bob", GameAction::Vote(real), 0).unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(
            game.act("bob", GameAction::Continue, 100),
            Err(GameError::NotHost)
        );
        game.act("alice", GameAction::Continue, 100).unwrap();
        assert_eq!((game.phase, game.round), (Phase::Collecting, 1));
    }

    #[test]
    fn rivalry_plays_recorded_at_reveal() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob"]);
        let penny = game.bot_ids[0].clone();
        game.act("alice", GameAction::Vote(author_opt(&game, &penny)), 0)
            .unwrap(); // alice fooled
        game.act("bob", GameAction::Vote(real_opt(&game)), 0)
            .unwrap(); // bob saw through it
        assert_eq!(game.phase, Phase::Reveal);
        let plays = game.take_round_plays();
        assert_eq!(plays.len(), 2);
        assert!(!plays.iter().find(|p| p.human_id == "alice").unwrap().caught);
        assert!(plays.iter().find(|p| p.human_id == "bob").unwrap().caught);
        assert!(game.take_round_plays().is_empty());
    }

    #[test]
    fn disconnect_nudges_collecting_then_voting() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        game.act("alice", GameAction::Submit("a".into()), 0)
            .unwrap();
        let penny = game.bot_ids[0].clone();
        game.apply_text(0, &penny, TextKind::Answer, "bluff".into(), 0);
        assert_eq!(game.phase, Phase::Collecting); // bob still owes a bluff
        game.set_connected("bob", false);
        game.nudge(0); // only remaining human + bot are in → Voting
        assert_eq!(game.phase, Phase::Voting);
    }

    #[test]
    fn pack_selects_its_bank() {
        for (pack, bank) in [
            (BalderdashPack::Movies, MOVIES),
            (BalderdashPack::Acronyms, ACRONYMS),
            (BalderdashPack::Laws, LAWS),
        ] {
            let mut game = Balderdash::new(
                "room1".into(),
                &id("alice"),
                RoomConfig::default(),
                seats(&["Penny"]),
                pack,
            );
            game.join(&id("alice")).unwrap();
            game.act("alice", GameAction::Start, 0).unwrap();
            let (subject, answer) = (game.word.unwrap(), game.real_def.unwrap());
            assert!(
                bank.iter().any(|(s, a)| *s == subject && *a == answer),
                "{pack:?}: {subject} not in its bank"
            );
        }
        // Words still maps difficulty to its tiered banks.
        let cfg = RoomConfig {
            difficulty: Difficulty::Hard,
            ..RoomConfig::default()
        };
        let mut g = Balderdash::new(
            "room1".into(),
            &id("alice"),
            cfg,
            seats(&["Penny"]),
            BalderdashPack::Words,
        );
        g.join(&id("alice")).unwrap();
        g.act("alice", GameAction::Start, 0).unwrap();
        assert!(HARD.iter().any(|(s, _)| *s == g.word.unwrap()));
    }
}
