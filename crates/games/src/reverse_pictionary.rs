use crate::driver::PlayerIdentity;
use crate::game::{
    is_correct, order_key, scoreboard, AiSeat, AiWork, Difficulty, Game, GameAction, GameError,
    Phase, PlayerId, RoomConfig, Roster, RoundArt,
};
use crate::protocol::{GameKind, GameView, GuessRow, RoomSummary, RoomView};

// Word banks by difficulty. Avoid leading articles and accents so the guess
// matcher stays forgiving. Easy = plain concrete nouns; up to Very Hard =
// abstract idioms that are genuinely tough to draw and guess.
const EASY: &[&str] = &[
    "cat", "house", "sun", "tree", "star", "fish", "car", "apple", "balloon", "umbrella", "guitar",
    "snowman", "boat", "flower", "hat", "key",
];

const NORMAL: &[&str] = &[
    "lighthouse",
    "rocket",
    "rainbow",
    "robot",
    "campfire",
    "treasure map",
    "haunted house",
    "hot air balloon",
    "ferris wheel",
    "waterfall",
    "scarecrow",
    "submarine",
];

const HARD: &[&str] = &[
    "couch potato",
    "night owl",
    "bookworm",
    "break the ice",
    "spill the beans",
    "cold feet",
    "raining cats and dogs",
    "piece of cake",
    "time travel",
    "homesick",
    "rush hour",
    "brain freeze",
];

const VERY_HARD: &[&str] = &[
    "cabin fever",
    "devils advocate",
    "elephant in the room",
    "cost an arm and a leg",
    "burning the midnight oil",
    "barking up the wrong tree",
    "once in a blue moon",
    "bite the bullet",
    "under the weather",
    "wild goose chase",
];

fn word_bank(difficulty: Difficulty) -> &'static [&'static str] {
    match difficulty {
        Difficulty::Easy => EASY,
        Difficulty::Normal => NORMAL,
        Difficulty::Hard => HARD,
        Difficulty::VeryHard => VERY_HARD,
    }
}

struct RoundImage {
    round: u32,
    bytes: Vec<u8>,
    mime: String,
}

/// Reverse Pictionary: the AI agent draws a secret word and humans race to guess it.
pub struct ReversePictionary {
    roster: Roster,
    config: RoomConfig,
    words: &'static [&'static str],
    artist: Option<AiSeat>,
    artist_id: Option<PlayerId>,
    phase: Phase,
    round: u32,
    deadline: i64,
    word_offset: usize,
    secret: Option<&'static str>,
    image: Option<RoundImage>,
    guessed: Vec<PlayerId>,
    unavailable: bool,
}

impl ReversePictionary {
    pub fn new(
        room_id: String,
        host: &PlayerIdentity,
        config: RoomConfig,
        artist: Option<AiSeat>,
    ) -> Self {
        let words = word_bank(config.difficulty);
        let word_offset = (order_key(&room_id, "") % words.len() as u64) as usize;
        Self {
            roster: Roster::new(room_id, host),
            config,
            words,
            artist,
            artist_id: None,
            phase: Phase::Lobby,
            round: 0,
            deadline: 0,
            word_offset,
            secret: None,
            image: None,
            guessed: Vec::new(),
            unavailable: false,
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
        let artist = self.artist.clone().ok_or(GameError::NoAiAvailable)?;
        self.artist_id = Some(self.roster.seat_ai(artist.agent_id, artist.persona));
        self.roster.assign_avatars();
        self.begin_round(0, now);
        Ok(())
    }

    fn guess(&mut self, actor: &str, text: &str, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Guessing {
            return Err(GameError::WrongPhase);
        }
        match self.roster.find(actor) {
            Some(p) if p.is_ai() => return Err(GameError::NotAPlayer),
            None => return Err(GameError::NotAPlayer),
            _ => {}
        }
        if self.guessed.iter().any(|g| g == actor) {
            return Ok(());
        }
        let Some(secret) = self.secret else {
            return Ok(());
        };
        if !is_correct(text, secret) {
            return Ok(());
        }
        let points = match self.guessed.len() {
            0 => 3,
            1 => 2,
            _ => 1,
        };
        self.guessed.push(actor.to_string());
        if let Some(p) = self.roster.find_mut(actor) {
            p.score += points;
        }
        if let Some(artist_id) = self.artist_id.clone() {
            if let Some(a) = self.roster.find_mut(&artist_id) {
                a.score += 1;
            }
        }
        if self.all_guessed() {
            self.begin_reveal(now);
        }
        Ok(())
    }

    fn begin_round(&mut self, round: u32, now: i64) {
        self.round = round;
        self.phase = Phase::Drawing;
        self.secret = Some(self.words[(self.word_offset + round as usize) % self.words.len()]);
        self.image = None;
        self.guessed.clear();
        self.unavailable = false;
        self.deadline = now + self.config.draw_secs;
    }

    fn begin_guessing(&mut self, now: i64) {
        self.phase = Phase::Guessing;
        // Easy mode is a relaxed guess; harder tiers lean on tougher words, not
        // a tighter clock (so they stay winnable).
        let secs = match self.config.difficulty {
            Difficulty::Easy => self.config.guess_secs * 3 / 2,
            _ => self.config.guess_secs,
        };
        self.deadline = now + secs;
    }

    fn begin_reveal(&mut self, now: i64) {
        self.phase = Phase::Reveal;
        self.deadline = now + self.config.reveal_secs;
    }

    /// The Reveal-arm transition: on to the next round, or end the game. Shared
    /// by the timer (`advance`) and the host's "Continue".
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

    fn all_guessed(&self) -> bool {
        self.roster
            .humans()
            .filter(|p| p.connected)
            .all(|p| self.guessed.contains(&p.id))
    }
}

impl Game for ReversePictionary {
    fn join(&mut self, identity: &PlayerIdentity) -> Result<PlayerId, GameError> {
        self.roster.join(identity, self.phase == Phase::Lobby)
    }

    fn set_connected(&mut self, id: &str, connected: bool) {
        self.roster.set_connected(id, connected);
    }

    fn act(&mut self, actor: &str, action: GameAction, now: i64) -> Result<(), GameError> {
        match action {
            GameAction::Start => self.start(actor, now),
            GameAction::Guess(text) => self.guess(actor, &text, now),
            GameAction::Continue => self.host_continue(actor, now),
            _ => Err(GameError::WrongPhase),
        }
    }

    fn advance(&mut self, now: i64) -> bool {
        if !matches!(self.phase, Phase::Drawing | Phase::Guessing | Phase::Reveal)
            || now < self.deadline
        {
            return false;
        }
        match self.phase {
            Phase::Drawing => {
                self.unavailable = true;
                self.begin_reveal(now);
            }
            Phase::Guessing => self.begin_reveal(now),
            Phase::Reveal => self.end_reveal(now),
            _ => unreachable!(),
        }
        true
    }

    fn pending_work(&self) -> Vec<AiWork> {
        if self.phase != Phase::Drawing || self.image.is_some() || self.unavailable {
            return Vec::new();
        }
        match (self.artist.as_ref(), self.secret) {
            (Some(artist), Some(secret)) => vec![AiWork::Image {
                round: self.round,
                agent_id: artist.agent_id.clone(),
                prompt: draw_prompt(secret, self.config.difficulty),
            }],
            _ => Vec::new(),
        }
    }

    fn apply_image(&mut self, round: u32, art: crate::driver::GeneratedArt, now: i64) {
        if round == self.round && self.phase == Phase::Drawing {
            self.image = Some(RoundImage {
                round,
                bytes: art.bytes,
                mime: art.mime,
            });
            self.begin_guessing(now);
        }
    }

    fn fail_work(&mut self, round: u32, now: i64) {
        if round == self.round && self.phase == Phase::Drawing {
            self.unavailable = true;
            self.begin_reveal(now);
        }
    }

    fn art(&self) -> Option<RoundArt<'_>> {
        let image = self.image.as_ref()?;
        Some(RoundArt {
            bytes: &image.bytes,
            mime: &image.mime,
        })
    }

    fn view(&self) -> RoomView {
        let revealing = matches!(self.phase, Phase::Reveal | Phase::GameOver);
        let guessed = self
            .guessed
            .iter()
            .enumerate()
            .filter_map(|(i, pid)| {
                self.roster.find(pid).map(|p| GuessRow {
                    player_id: p.id.clone(),
                    label: p.display_name.clone(),
                    avatar: p.avatar.clone(),
                    rank: i as u32 + 1,
                })
            })
            .collect();

        RoomView {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::ReversePictionary,
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
            game: GameView::ReversePictionary {
                artist: self
                    .artist
                    .as_ref()
                    .map(|a| a.persona.clone())
                    .unwrap_or_default(),
                art_round: self.image.as_ref().map(|img| img.round),
                guessed,
                secret: revealing.then(|| self.secret.unwrap_or_default().to_string()),
                unavailable: self.unavailable,
            },
        }
    }

    fn summary(&self) -> RoomSummary {
        RoomSummary {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::ReversePictionary,
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

fn draw_prompt(secret: &str, difficulty: Difficulty) -> String {
    let subject = match difficulty {
        Difficulty::Easy => format!(
            "a clear, friendly, easily recognizable doodle of \"{secret}\" — simple but unmistakable"
        ),
        Difficulty::Normal => format!(
            "a rough, simple, minimalist doodle of \"{secret}\" — like a quick 10-second scribble \
             someone dashes off in a party game; a little sketchy, guessing it should take a moment \
             of thought"
        ),
        Difficulty::Hard | Difficulty::VeryHard => format!(
            "a crude, sketchy, deliberately ambiguous scribble of \"{secret}\" — barely more than a \
             quick gesture, not a polished or obviously labelled illustration (it should be \
             genuinely hard to guess)"
        ),
    };
    format!(
        "{subject}. Draw it literally — the literal objects, even if it's a phrase or idiom. Plain \
         white background. Absolutely NO text, letters, words, or labels anywhere in the image."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::GeneratedArt;

    fn id(name: &str) -> PlayerIdentity {
        PlayerIdentity {
            id: name.to_string(),
            display_name: name.to_uppercase(),
        }
    }

    fn artist() -> Option<AiSeat> {
        Some(AiSeat {
            agent_id: "penny".into(),
            persona: "Penny".into(),
        })
    }

    fn art(prompt: &str) -> GeneratedArt {
        GeneratedArt {
            bytes: format!("PNG:{prompt}").into_bytes(),
            mime: "image/png".into(),
        }
    }

    fn started(humans: &[&str]) -> ReversePictionary {
        let mut game = ReversePictionary::new(
            "room1".into(),
            &id(humans[0]),
            RoomConfig::default(),
            artist(),
        );
        for h in humans {
            game.join(&id(h)).unwrap();
        }
        game.act("alice", GameAction::Start, 0).unwrap();
        game
    }

    #[test]
    fn start_begins_drawing_and_owes_an_image() {
        let game = started(&["alice", "bob"]);
        assert_eq!(game.phase, Phase::Drawing);
        match game
            .pending_work()
            .into_iter()
            .next()
            .expect("owes an image")
        {
            AiWork::Image {
                agent_id, prompt, ..
            } => {
                assert_eq!(agent_id, "penny");
                assert!(prompt.contains("doodle"));
            }
            _ => panic!("expected image work"),
        }
    }

    #[test]
    fn image_unlocks_guessing_and_is_served() {
        let mut game = started(&["alice", "bob"]);
        let AiWork::Image { round, prompt, .. } = game.pending_work().into_iter().next().unwrap()
        else {
            panic!();
        };
        game.apply_image(round, art(&prompt), 0);
        assert_eq!(game.phase, Phase::Guessing);
        assert!(game.pending_work().is_empty());
        let served = game.art().expect("art served");
        assert_eq!(served.mime, "image/png");
    }

    #[test]
    fn correct_guess_scores_speed_tiered_and_artist_gains() {
        let mut game = started(&["alice", "bob", "carol"]);
        let secret = game.secret.unwrap();
        let AiWork::Image { round, prompt, .. } = game.pending_work().into_iter().next().unwrap()
        else {
            panic!();
        };
        game.apply_image(round, art(&prompt), 0);
        // first correct (with punctuation/case) → 3 pts
        game.act(
            "bob",
            GameAction::Guess(format!("it's a {}!", secret.to_uppercase())),
            0,
        )
        .unwrap();
        // wrong guess → no-op
        game.act("carol", GameAction::Guess("banana".into()), 0)
            .unwrap();
        // second correct → 2 pts
        game.act("carol", GameAction::Guess(secret.into()), 0)
            .unwrap();
        assert_eq!(game.roster.find("bob").unwrap().score, 3);
        assert_eq!(game.roster.find("carol").unwrap().score, 2);
        let artist_id = game.artist_id.clone().unwrap();
        assert_eq!(game.roster.find(&artist_id).unwrap().score, 2);
    }

    #[test]
    fn all_guessed_ends_round_and_hides_secret_until_reveal() {
        let mut game = started(&["alice"]);
        let secret = game.secret.unwrap();
        let AiWork::Image { round, prompt, .. } = game.pending_work().into_iter().next().unwrap()
        else {
            panic!();
        };
        game.apply_image(round, art(&prompt), 0);
        match game.view().game {
            GameView::ReversePictionary {
                secret: s,
                art_round,
                ..
            } => {
                assert!(s.is_none());
                assert_eq!(art_round, Some(round));
            }
            _ => panic!("wrong view"),
        }
        game.act("alice", GameAction::Guess(secret.into()), 0)
            .unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        match game.view().game {
            GameView::ReversePictionary { secret: s, .. } => assert_eq!(s.as_deref(), Some(secret)),
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn failed_drawing_skips_to_reveal_as_unavailable() {
        let mut game = started(&["alice"]);
        game.fail_work(game.round, 0);
        assert_eq!(game.phase, Phase::Reveal);
        match game.view().game {
            GameView::ReversePictionary { unavailable, .. } => assert!(unavailable),
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn timer_drawing_timeout_then_progresses_to_game_over() {
        let cfg = RoomConfig {
            total_rounds: 1,
            draw_secs: 10,
            guess_secs: 10,
            reveal_secs: 5,
            ..RoomConfig::default()
        };
        let mut game = ReversePictionary::new("room1".into(), &id("alice"), cfg, artist());
        game.join(&id("alice")).unwrap();
        game.act("alice", GameAction::Start, 0).unwrap();
        assert!(!game.advance(5));
        assert!(game.advance(10)); // drawing timed out → reveal (unavailable)
        assert_eq!(game.phase, Phase::Reveal);
        assert!(game.advance(15)); // reveal → game over (1 round)
        assert_eq!(game.phase, Phase::GameOver);
    }

    #[test]
    fn guess_matching_normalises() {
        assert!(is_correct("ROCKET!", "rocket"));
        assert!(is_correct("it is a cat", "cat"));
        assert!(is_correct("ice-cream", "ice cream"));
        assert!(is_correct("it's a night owl!", "night owl"));
        assert!(!is_correct("dog", "cat"));
        assert!(!is_correct("category", "cat"));
        assert!(!is_correct("owl", "night owl"));
    }

    #[test]
    fn stale_or_wrong_phase_image_is_ignored() {
        let mut game = started(&["alice"]);
        let AiWork::Image { round, prompt, .. } = game.pending_work().into_iter().next().unwrap()
        else {
            panic!();
        };
        game.apply_image(round + 5, art(&prompt), 0);
        assert_eq!(game.phase, Phase::Drawing);
        assert!(game.art().is_none());
        game.fail_work(round + 5, 0);
        assert_eq!(game.phase, Phase::Drawing);
        game.apply_image(round, art(&prompt), 0);
        assert_eq!(game.phase, Phase::Guessing);
        // a late image after we've already left Drawing is ignored
        game.apply_image(round, art("late"), 0);
        assert_eq!(game.phase, Phase::Guessing);
    }

    #[test]
    fn difficulty_selects_word_bank_deterministically() {
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
            let mut game = ReversePictionary::new("room1".into(), &id("alice"), cfg, artist());
            game.join(&id("alice")).unwrap();
            game.act("alice", GameAction::Start, 0).unwrap();
            let secret = game.secret.unwrap();
            assert!(
                bank.contains(&secret),
                "{difficulty:?}: {secret} not in bank"
            );

            // Same room + difficulty → same secret (deterministic).
            let cfg = RoomConfig {
                difficulty,
                ..RoomConfig::default()
            };
            let mut again = ReversePictionary::new("room1".into(), &id("alice"), cfg, artist());
            again.join(&id("alice")).unwrap();
            again.act("alice", GameAction::Start, 0).unwrap();
            assert_eq!(again.secret.unwrap(), secret);
        }
    }

    #[test]
    fn host_continue_skips_reveal_and_rejects_non_host() {
        let cfg = RoomConfig {
            total_rounds: 2,
            ..RoomConfig::default()
        };
        let mut game = ReversePictionary::new("room1".into(), &id("alice"), cfg, artist());
        game.join(&id("alice")).unwrap();
        game.join(&id("bob")).unwrap();
        game.act("alice", GameAction::Start, 0).unwrap();
        game.fail_work(game.round, 0); // drawing unavailable → reveal
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(
            game.act("bob", GameAction::Continue, 0),
            Err(GameError::NotHost)
        );
        game.act("alice", GameAction::Continue, 0).unwrap();
        assert_eq!((game.phase, game.round), (Phase::Drawing, 1));
        // last round's reveal → Continue ends the game
        game.fail_work(game.round, 0);
        game.act("alice", GameAction::Continue, 0).unwrap();
        assert_eq!(game.phase, Phase::GameOver);
        assert_eq!(
            game.act("alice", GameAction::Continue, 0),
            Err(GameError::WrongPhase)
        );
    }
}
