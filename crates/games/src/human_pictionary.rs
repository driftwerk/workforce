use crate::driver::PlayerIdentity;
use crate::game::{
    is_correct, scoreboard, AiSeat, AiWork, Difficulty, Game, GameAction, GameError, Phase,
    PlayerId, RoomConfig, Roster, RoundArt,
};
use crate::protocol::{GameKind, GameView, GuessFeedRow, RoomSummary, RoomView, RoundGuessRow};

// Drawable words live in `wordlists/*.txt` (one per difficulty tier, ~1,800 in
// total), embedded at compile time. Each round picks one at random with no repeats
// within a game, so play stays fresh. Very hard is abstract on purpose.
fn word_bank(difficulty: Difficulty) -> Vec<&'static str> {
    let raw = match difficulty {
        Difficulty::Easy => include_str!("wordlists/easy.txt"),
        Difficulty::Normal => include_str!("wordlists/normal.txt"),
        Difficulty::Hard => include_str!("wordlists/hard.txt"),
        Difficulty::VeryHard => include_str!("wordlists/very-hard.txt"),
    };
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

/// How long to wait between AI guesses, and the most it may guess per round.
const GUESS_THROTTLE_SECS: i64 = 5;
const MAX_GUESSES_PER_ROUND: u32 = 8;
/// How many words the drawer picks between at the start of their turn (Skribbl-style).
const WORD_CHOICES: usize = 5;
/// Recent chat lines (guesses + banter) kept for the live feed.
const MAX_FEED: usize = 40;

/// Round scoring — earlier correct guesses score more; the drawer earns per
/// teammate who solves it (so a *guessable* drawing is the skill). Tune freely.
const FIRST_GUESS_POINTS: u32 = 5;
const MIN_GUESS_POINTS: u32 = 2;
const DRAWER_POINTS_PER_GUESSER: u32 = 2;

/// Points for the Nth (0-based) correct guesser this round: 5, 4, 3, then 2.
fn guess_points(rank: usize) -> u32 {
    (FIRST_GUESS_POINTS as i32 - rank as i32).max(MIN_GUESS_POINTS as i32) as u32
}

struct Snapshot {
    round: u32,
    bytes: Vec<u8>,
    mime: String,
}

/// Human Pictionary: each round a rotating human draws a secret word on a canvas
/// while the AI and the other humans race to guess it. Everyone who solves it
/// scores (earlier is worth more), and the round runs until all humans have it.
pub struct HumanPictionary {
    roster: Roster,
    config: RoomConfig,
    words: Vec<&'static str>,
    used: Vec<&'static str>,
    guesser: Option<AiSeat>,
    guesser_id: Option<PlayerId>,
    phase: Phase,
    round: u32,
    deadline: i64,
    secret: Option<&'static str>,
    choices: Vec<&'static str>,
    drawer_id: Option<PlayerId>,
    image: Option<Snapshot>,
    display_snapshot: u32,
    awaiting_guess: bool,
    last_guessed_snapshot: u32,
    last_guessed_at: i64,
    guessable_snapshot: u32,
    guess_count: u32,
    /// Who has solved the word this round, in guess order (humans and the AI).
    correct_round: Vec<PlayerId>,
    unavailable: bool,
    guesses: Vec<GuessFeedRow>,
}

impl HumanPictionary {
    pub fn new(
        room_id: String,
        host: &PlayerIdentity,
        config: RoomConfig,
        guesser: Option<AiSeat>,
    ) -> Self {
        let words = word_bank(config.difficulty);
        Self {
            roster: Roster::new(room_id, host),
            config,
            words,
            used: Vec::new(),
            guesser,
            guesser_id: None,
            phase: Phase::Lobby,
            round: 0,
            deadline: 0,
            secret: None,
            choices: Vec::new(),
            drawer_id: None,
            image: None,
            display_snapshot: 0,
            awaiting_guess: false,
            last_guessed_snapshot: 0,
            last_guessed_at: 0,
            guessable_snapshot: 0,
            guess_count: 0,
            correct_round: Vec::new(),
            unavailable: false,
            guesses: Vec::new(),
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
        // The AI guesser is optional — humans can play without it.
        if let Some(seat) = self.guesser.clone() {
            self.guesser_id = Some(self.roster.seat_ai(seat.agent_id, seat.persona));
        }
        self.roster.assign_avatars();
        self.begin_round(0, now);
        Ok(())
    }

    /// `WORD_CHOICES` distinct words for the drawer to pick from — none marked
    /// used until one is committed. The used pool resets if too few remain, so for
    /// a non-empty bank the list is always `min(WORD_CHOICES, bank)` long. Entropy
    /// comes from a v4 UUID — no extra dependency.
    fn pick_choices(&mut self) -> Vec<&'static str> {
        let want = WORD_CHOICES.min(self.words.len());
        if self.words.len() - self.used.len() < want {
            self.used.clear();
        }
        let mut chosen: Vec<&'static str> = Vec::with_capacity(want);
        while chosen.len() < want {
            let idx = (uuid::Uuid::new_v4().as_u128() % self.words.len() as u128) as usize;
            let word = self.words[idx];
            if !self.used.contains(&word) && !chosen.contains(&word) {
                chosen.push(word);
            }
        }
        chosen
    }

    /// The round's drawer rotates across the *connected* humans.
    fn pick_drawer(&self, round: u32) -> Option<PlayerId> {
        let connected: Vec<&PlayerId> = self
            .roster
            .players
            .iter()
            .filter(|p| !p.is_ai() && p.connected)
            .map(|p| &p.id)
            .collect();
        if connected.is_empty() {
            return None;
        }
        Some(connected[(round as usize) % connected.len()].clone())
    }

    fn begin_round(&mut self, round: u32, now: i64) {
        self.round = round;
        self.secret = None;
        self.choices = Vec::new();
        self.drawer_id = self.pick_drawer(round);
        self.image = None;
        self.display_snapshot = 0;
        self.awaiting_guess = false;
        self.last_guessed_snapshot = 0;
        self.last_guessed_at = 0;
        self.guessable_snapshot = 0;
        self.guess_count = 0;
        self.correct_round.clear();
        self.guesses.clear();
        self.unavailable = self.drawer_id.is_none();
        if self.unavailable {
            self.begin_reveal(now);
        } else {
            self.begin_choosing(now);
        }
    }

    /// The drawer's turn opens on a private word shortlist; they draw only once
    /// they commit (or the timer auto-picks for them).
    fn begin_choosing(&mut self, now: i64) {
        self.phase = Phase::Choosing;
        self.choices = self.pick_choices();
        self.deadline = now + self.config.choose_secs;
    }

    fn begin_drawing(&mut self, now: i64) {
        self.phase = Phase::Drawing;
        self.deadline = now + self.config.draw_secs;
    }

    /// The drawer commits to one of the offered words, then starts drawing.
    fn choose(&mut self, actor: &str, word: &str, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Choosing {
            return Err(GameError::WrongPhase);
        }
        if self.drawer_id.as_deref() != Some(actor) {
            return Err(GameError::NotDrawer);
        }
        let chosen = self
            .choices
            .iter()
            .copied()
            .find(|w| *w == word)
            .ok_or(GameError::UnknownTarget)?;
        self.commit_word(chosen, now);
        Ok(())
    }

    /// Choosing timed out — pick the drawer's first word for them.
    fn auto_choose(&mut self, now: i64) {
        match self.choices.first().copied() {
            Some(first) => self.commit_word(first, now),
            None => self.begin_reveal(now),
        }
    }

    fn commit_word(&mut self, word: &'static str, now: i64) {
        self.used.push(word);
        self.secret = Some(word);
        self.choices = Vec::new();
        self.begin_drawing(now);
    }

    fn begin_reveal(&mut self, now: i64) {
        self.phase = Phase::Reveal;
        self.deadline = now + self.config.reveal_secs;
    }

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

    fn has_guessed(&self, id: &str) -> bool {
        self.correct_round.iter().any(|p| p == id)
    }

    /// True once every connected non-drawer human has solved it (and there is at
    /// least one) — the cue to end the round early. The AI never gates this.
    fn all_humans_guessed(&self) -> bool {
        let mut any = false;
        for p in &self.roster.players {
            if p.is_ai() || !p.connected || self.drawer_id.as_deref() == Some(p.id.as_str()) {
                continue;
            }
            any = true;
            if !self.has_guessed(&p.id) {
                return false;
            }
        }
        any
    }

    fn maybe_end(&mut self, now: i64) {
        if self.phase == Phase::Drawing && self.all_humans_guessed() {
            self.begin_reveal(now);
        }
    }

    /// A connected non-drawer human who still hasn't solved it — i.e. someone the
    /// round is still waiting on.
    fn unsolved_humans_remain(&self) -> bool {
        self.roster.players.iter().any(|p| {
            !p.is_ai()
                && p.connected
                && self.drawer_id.as_deref() != Some(p.id.as_str())
                && !self.has_guessed(&p.id)
        })
    }

    /// Score a correct guess: the guesser by speed (5/4/3/2…), the drawer +2 for
    /// landing another solver, then check whether the round is done.
    fn score_correct(&mut self, guesser_id: &str, now: i64) {
        let points = guess_points(self.correct_round.len());
        if let Some(g) = self.roster.find_mut(guesser_id) {
            g.score += points;
        }
        if let Some(drawer) = self.drawer_id.clone() {
            if let Some(d) = self.roster.find_mut(&drawer) {
                d.score += DRAWER_POINTS_PER_GUESSER;
            }
        }
        self.correct_round.push(guesser_id.to_string());
        self.maybe_end(now);
    }

    /// A player's chat line. Guesses and banter share one stream — a correct
    /// guess is recorded with its text blanked so it can't leak the word.
    fn submit(&mut self, actor: &str, text: &str, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Drawing {
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
        let is_drawer = self.drawer_id.as_deref() == Some(actor);
        let solved = is_drawer || self.has_guessed(actor);
        let matches = self.secret.map(|s| is_correct(&text, s)).unwrap_or(false);
        if matches {
            // Anyone who already knows the word (drawer or a solver) typing it is a
            // spoiler — silently drop it so it never reaches the guessers' chat.
            if solved {
                return Ok(());
            }
            self.record_chat(actor, String::new(), true, false);
            self.score_correct(actor, now);
            return Ok(());
        }
        // Ordinary chat (this includes a guesser's wrong guess).
        self.record_chat(actor, text, false, solved);
        Ok(())
    }

    fn record_chat(&mut self, player_id: &str, text: String, correct: bool, solved: bool) {
        let Some((label, avatar, is_ai)) = self
            .roster
            .find(player_id)
            .map(|p| (p.display_name.clone(), p.avatar.clone(), p.is_ai()))
        else {
            return;
        };
        self.guesses.push(GuessFeedRow {
            player_id: player_id.to_string(),
            label,
            avatar,
            text,
            correct,
            is_ai,
            solved,
        });
        let overflow = self.guesses.len().saturating_sub(MAX_FEED);
        if overflow > 0 {
            self.guesses.drain(0..overflow);
        }
    }

    /// A fresh snapshot may earn the AI one (throttled, capped) vision guess —
    /// gated by `awaiting_guess` so only one call is ever in flight, and stopped
    /// once the AI has solved it.
    fn maybe_owe_guess(&mut self, now: i64) {
        let ai_unsolved = self
            .guesser_id
            .as_deref()
            .map(|id| !self.has_guessed(id))
            .unwrap_or(false);
        if self.guesser.is_some()
            && ai_unsolved
            && !self.awaiting_guess
            && self.guess_count < MAX_GUESSES_PER_ROUND
            && now - self.last_guessed_at >= GUESS_THROTTLE_SECS
            && self.display_snapshot > self.last_guessed_snapshot
        {
            self.guessable_snapshot = self.display_snapshot;
            self.awaiting_guess = true;
            self.guess_count += 1;
        }
    }

    /// The round's solvers, in order, with the points each earned.
    fn correct_rows(&self) -> Vec<RoundGuessRow> {
        self.correct_round
            .iter()
            .enumerate()
            .filter_map(|(rank, id)| {
                let p = self.roster.find(id)?;
                Some(RoundGuessRow {
                    player_id: p.id.clone(),
                    label: p.display_name.clone(),
                    avatar: p.avatar.clone(),
                    is_ai: p.is_ai(),
                    points: guess_points(rank),
                })
            })
            .collect()
    }
}

impl Game for HumanPictionary {
    fn join(&mut self, identity: &PlayerIdentity) -> Result<PlayerId, GameError> {
        self.roster.join(identity, self.phase == Phase::Lobby)
    }

    fn set_connected(&mut self, id: &str, connected: bool) {
        self.roster.set_connected(id, connected);
    }

    fn act(&mut self, actor: &str, action: GameAction, now: i64) -> Result<(), GameError> {
        match action {
            GameAction::Start => self.start(actor, now),
            GameAction::Guess(text) => self.submit(actor, &text, now),
            GameAction::Choose(word) => self.choose(actor, &word, now),
            GameAction::Continue => self.host_continue(actor, now),
            _ => Err(GameError::WrongPhase),
        }
    }

    fn advance(&mut self, now: i64) -> bool {
        if !matches!(self.phase, Phase::Choosing | Phase::Drawing | Phase::Reveal)
            || now < self.deadline
        {
            return false;
        }
        match self.phase {
            // The drawer dithered — pick a word and start their turn.
            Phase::Choosing => self.auto_choose(now),
            // Time's up with no winner — reveal the unguessed word.
            Phase::Drawing => self.begin_reveal(now),
            Phase::Reveal => self.end_reveal(now),
            _ => unreachable!(),
        }
        true
    }

    fn pending_work(&self) -> Vec<AiWork> {
        if self.phase != Phase::Drawing || !self.awaiting_guess {
            return Vec::new();
        }
        match (self.guesser.as_ref(), self.guesser_id.as_ref()) {
            (Some(seat), Some(pid)) => vec![AiWork::Vision {
                round: self.round,
                snapshot: self.guessable_snapshot,
                player_id: pid.clone(),
                agent_id: seat.agent_id.clone(),
                prompt: guess_prompt(),
            }],
            _ => Vec::new(),
        }
    }

    fn apply_drawing(
        &mut self,
        drawer_id: &str,
        png: Vec<u8>,
        round: Option<u32>,
        seq: Option<u32>,
        now: i64,
    ) -> Result<(), GameError> {
        if self.phase != Phase::Drawing {
            return Err(GameError::WrongPhase);
        }
        if self.drawer_id.as_deref() != Some(drawer_id) {
            return Err(GameError::NotDrawer);
        }
        // Drop a straggler from a previous round (reachable only for a solo
        // repeat-drawer) or a stale/out-of-order snapshot, so the displayed image
        // only ever moves forward. A seq-less upload keeps the old increment.
        if matches!(round, Some(r) if r != self.round) {
            return Ok(());
        }
        let seq = seq.unwrap_or(self.display_snapshot + 1);
        if seq <= self.display_snapshot {
            return Ok(());
        }
        self.image = Some(Snapshot {
            round: self.round,
            bytes: png,
            mime: "image/png".to_string(),
        });
        self.display_snapshot = seq;
        self.maybe_owe_guess(now);
        Ok(())
    }

    fn choices_for(&self, player_id: &str) -> Vec<String> {
        if self.phase == Phase::Choosing && self.drawer_id.as_deref() == Some(player_id) {
            self.choices.iter().map(|w| w.to_string()).collect()
        } else {
            Vec::new()
        }
    }

    fn apply_guess(&mut self, round: u32, player_id: &str, text: String, now: i64) {
        if round != self.round || self.guesser_id.as_deref() != Some(player_id) {
            return;
        }
        // Free the in-flight slot regardless of the result.
        self.awaiting_guess = false;
        self.last_guessed_snapshot = self.guessable_snapshot;
        self.last_guessed_at = now;
        let text = text.trim().to_string();
        if text.is_empty() || self.phase != Phase::Drawing || self.has_guessed(player_id) {
            return;
        }
        let correct = self.secret.map(|s| is_correct(&text, s)).unwrap_or(false);
        if correct {
            self.record_chat(player_id, String::new(), true, false);
            self.score_correct(player_id, now);
        } else {
            self.record_chat(player_id, text, false, false);
        }
    }

    fn word_for(&self, player_id: &str) -> Option<&str> {
        if self.phase == Phase::Drawing && self.drawer_id.as_deref() == Some(player_id) {
            self.secret
        } else {
            None
        }
    }

    fn nudge(&mut self, now: i64) {
        if !matches!(self.phase, Phase::Choosing | Phase::Drawing) {
            return;
        }
        // If the drawer leaves while choosing or drawing, skip rather than hang.
        let drawer_gone = match &self.drawer_id {
            Some(id) => !self.roster.find(id).map(|p| p.connected).unwrap_or(false),
            None => true,
        };
        if drawer_gone {
            self.unavailable = true;
            self.choices = Vec::new();
            self.begin_reveal(now);
            return;
        }
        // The departing player may have been the last one still guessing — whether
        // because everyone left has solved it or because no guessers remain at all.
        if self.phase == Phase::Drawing && !self.unsolved_humans_remain() {
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
        let drawer_label = self
            .drawer_id
            .as_ref()
            .and_then(|id| self.roster.find(id))
            .map(|p| p.display_name.clone());

        RoomView {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::HumanPictionary,
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
            game: GameView::HumanPictionary {
                drawer_id: self.drawer_id.clone(),
                drawer_label,
                art_round: self.image.as_ref().map(|i| i.round),
                snapshot: self.display_snapshot,
                guesses: self.guesses.clone(),
                correct: self.correct_rows(),
                drawer_points: DRAWER_POINTS_PER_GUESSER * self.correct_round.len() as u32,
                secret: revealing.then(|| self.secret.unwrap_or_default().to_string()),
                unavailable: self.unavailable,
            },
        }
    }

    fn summary(&self) -> RoomSummary {
        RoomSummary {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::HumanPictionary,
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

fn guess_prompt() -> String {
    "You're playing Pictionary. Someone is drawing a single word on a canvas. Look at this \
     in-progress drawing and reply with ONLY your single best one-or-two-word guess of what it \
     depicts — no sentences, no preamble. If you genuinely can't tell yet, reply with nothing."
        .to_string()
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

    fn guesser() -> Option<AiSeat> {
        Some(AiSeat {
            agent_id: "penny_game".into(),
            persona: "Penny".into(),
        })
    }

    /// The drawer commits to their first offered word, opening the Drawing phase.
    fn choose_first(game: &mut HumanPictionary) {
        let drawer = game.drawer_id.clone().unwrap();
        let word = game.choices[0].to_string();
        game.act(&drawer, GameAction::Choose(word), 0).unwrap();
    }

    fn started(humans: &[&str]) -> HumanPictionary {
        let mut game = at_choosing(humans);
        choose_first(&mut game);
        game
    }

    #[test]
    fn drawer_rotates_and_word_is_private_to_the_drawer() {
        let mut game = started(&["alice", "bob"]);
        let secret = game.secret.unwrap();
        // round 0 drawer is the first connected human
        let drawer = game.drawer_id.clone().unwrap();
        assert_eq!(game.word_for(&drawer), Some(secret));
        let other = if drawer == "alice" { "bob" } else { "alice" };
        assert_eq!(game.word_for(other), None);
        // the broadcast view never carries the word until reveal
        match game.view().game {
            GameView::HumanPictionary { secret, .. } => assert!(secret.is_none()),
            _ => panic!("wrong view"),
        }
        // next round rotates the drawer
        game.begin_round(1, 0);
        assert_ne!(game.drawer_id, Some(drawer));
    }

    #[test]
    fn a_guesser_scores_and_the_drawer_scores() {
        let mut game = started(&["alice", "bob"]);
        let secret = game.secret.unwrap().to_string();
        let drawer = game.drawer_id.clone().unwrap();
        let guesser = if drawer == "alice" { "bob" } else { "alice" };
        // The drawer typing the word is a spoiler — dropped, never scored or shown.
        game.act(&drawer, GameAction::Guess(secret.clone()), 0)
            .unwrap();
        assert!(game.correct_round.is_empty());
        assert!(game.guesses.is_empty());
        // The only other human guesses correctly → first solver +5, drawer +2, and
        // since every human now has it the round ends.
        game.act(guesser, GameAction::Guess(format!("it's a {secret}!")), 0)
            .unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(game.roster.find(guesser).unwrap().score, 5);
        assert_eq!(game.roster.find(&drawer).unwrap().score, 2);
    }

    #[test]
    fn apply_drawing_rejects_non_drawer_and_wrong_phase() {
        let mut game = started(&["alice", "bob"]);
        let drawer = game.drawer_id.clone().unwrap();
        let other = if drawer == "alice" { "bob" } else { "alice" };
        assert_eq!(
            game.apply_drawing(other, b"png".to_vec(), None, None, 0),
            Err(GameError::NotDrawer)
        );
        game.apply_drawing(&drawer, b"png".to_vec(), None, None, 0)
            .unwrap();
        assert!(game.art().is_some());
    }

    #[test]
    fn vision_guess_is_throttled_to_one_in_flight() {
        let mut game = started(&["alice", "bob"]);
        let drawer = game.drawer_id.clone().unwrap();
        // first snapshot owes a guess
        game.apply_drawing(&drawer, b"a".to_vec(), None, None, 10)
            .unwrap();
        assert_eq!(game.pending_work().len(), 1);
        assert!(game.awaiting_guess);
        // another snapshot while one is in flight → no new work
        game.apply_drawing(&drawer, b"b".to_vec(), None, None, 11)
            .unwrap();
        assert_eq!(game.pending_work().len(), 1);
        // the AI answers (wrong) → slot freed, but throttle holds within 5s
        let pid = game.guesser_id.clone().unwrap();
        game.apply_guess(0, &pid, "house".into(), 12);
        assert!(!game.awaiting_guess);
        game.apply_drawing(&drawer, b"c".to_vec(), None, None, 13)
            .unwrap();
        assert!(!game.awaiting_guess); // throttled (within 5s of last guess)
                                       // past the throttle, a fresh snapshot owes again
        game.apply_drawing(&drawer, b"d".to_vec(), None, None, 20)
            .unwrap();
        assert!(game.awaiting_guess);
        assert_eq!(game.pending_work().len(), 1);
    }

    #[test]
    fn ai_guess_scores_without_ending_the_round() {
        let mut game = started(&["alice", "bob"]);
        let secret = game.secret.unwrap().to_string();
        let drawer = game.drawer_id.clone().unwrap();
        let pid = game.guesser_id.clone().unwrap();
        game.apply_drawing(&drawer, b"a".to_vec(), None, None, 10)
            .unwrap();
        game.apply_guess(0, &pid, secret, 11);
        // The AI solved it (+5) but a human still hasn't — the round plays on, and
        // its correct line carries no text (so it can't leak the word).
        assert_eq!(game.phase, Phase::Drawing);
        assert_eq!(game.roster.find(&pid).unwrap().score, 5);
        assert!(game.has_guessed(&pid));
        assert!(game.guesses.last().unwrap().correct);
        assert!(game.guesses.last().unwrap().text.is_empty());
    }

    #[test]
    fn drawer_disconnect_skips_the_round() {
        let mut game = started(&["alice", "bob"]);
        let drawer = game.drawer_id.clone().unwrap();
        game.set_connected(&drawer, false);
        game.nudge(0);
        assert_eq!(game.phase, Phase::Reveal);
        match game.view().game {
            GameView::HumanPictionary { unavailable, .. } => assert!(unavailable),
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn timeout_reveals_unguessed_word() {
        let cfg = RoomConfig {
            choose_secs: 5,
            draw_secs: 10,
            reveal_secs: 5,
            total_rounds: 1,
            ..RoomConfig::default()
        };
        let mut game = HumanPictionary::new("room1".into(), &id("alice"), cfg, guesser());
        game.join(&id("alice")).unwrap();
        game.join(&id("bob")).unwrap();
        game.act("alice", GameAction::Start, 0).unwrap();
        assert_eq!(game.phase, Phase::Choosing);
        assert!(game.advance(5)); // choosing timed out → auto-pick → drawing
        assert_eq!(game.phase, Phase::Drawing);
        assert!(game.secret.is_some());
        assert!(game.advance(15)); // drawing timed out → reveal
        assert_eq!(game.phase, Phase::Reveal);
        assert!(game.correct_round.is_empty());
        assert!(game.advance(20)); // reveal → game over (1 round)
        assert_eq!(game.phase, Phase::GameOver);
    }

    fn non_drawers(game: &HumanPictionary) -> Vec<String> {
        let drawer = game.drawer_id.clone().unwrap();
        ["alice", "bob", "carol"]
            .into_iter()
            .filter(|h| *h != drawer)
            .map(String::from)
            .collect()
    }

    #[test]
    fn each_guesser_scores_by_speed_and_the_round_waits_for_all() {
        let mut game = started(&["alice", "bob", "carol"]);
        let secret = game.secret.unwrap().to_string();
        let drawer = game.drawer_id.clone().unwrap();
        let others = non_drawers(&game);
        // First human solves → +5, drawer +2; another human hasn't → play continues.
        game.act(&others[0], GameAction::Guess(secret.clone()), 0)
            .unwrap();
        assert_eq!(game.phase, Phase::Drawing);
        assert_eq!(game.roster.find(&others[0]).unwrap().score, 5);
        // Second solver → +4, drawer +2 again; everyone has it now → reveal.
        game.act(&others[1], GameAction::Guess(secret), 0).unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(game.roster.find(&others[1]).unwrap().score, 4);
        assert_eq!(game.roster.find(&drawer).unwrap().score, 4); // +2 per solver
    }

    #[test]
    fn round_ends_when_the_last_unguessed_human_leaves() {
        let mut game = started(&["alice", "bob", "carol"]);
        let secret = game.secret.unwrap().to_string();
        let others = non_drawers(&game);
        game.act(&others[0], GameAction::Guess(secret), 0).unwrap();
        assert_eq!(game.phase, Phase::Drawing); // others[1] is still guessing
                                                // The last un-guessed human disconnects → the round resolves rather than
                                                // waiting out the timer.
        game.set_connected(&others[1], false);
        game.nudge(0);
        assert_eq!(game.phase, Phase::Reveal);
    }

    #[test]
    fn round_ends_when_the_only_unsolved_guesser_leaves() {
        // Drawer + a single guesser who never solves it: when they leave there is
        // no one left to wait on, so the round resolves rather than stalling.
        let mut game = started(&["alice", "bob"]);
        let drawer = game.drawer_id.clone().unwrap();
        let guesser = if drawer == "alice" { "bob" } else { "alice" };
        game.set_connected(guesser, false);
        game.nudge(0);
        assert_eq!(game.phase, Phase::Reveal);
    }

    #[test]
    fn chat_flows_but_no_one_who_knows_the_word_can_spoil_it() {
        let mut game = started(&["alice", "bob", "carol"]);
        let secret = game.secret.unwrap().to_string();
        let drawer = game.drawer_id.clone().unwrap();
        let others = non_drawers(&game);
        // A guesser's banter shows in the chat as an ordinary line.
        game.act(&others[0], GameAction::Guess("hmm a tricky one".into()), 0)
            .unwrap();
        assert_eq!(game.guesses.len(), 1);
        assert!(!game.guesses[0].correct);
        // The drawer knows the word — typing it is dropped, never broadcast.
        game.act(&drawer, GameAction::Guess(secret.clone()), 0)
            .unwrap();
        assert_eq!(game.guesses.len(), 1);
        // others[0] solves it; the feed line is flagged correct with blanked text.
        game.act(&others[0], GameAction::Guess(secret.clone()), 0)
            .unwrap();
        let solved = game.guesses.last().unwrap();
        assert!(solved.correct && solved.text.is_empty());
        // A solver typing the word again is dropped too — no spoiling the rest.
        let before = game.guesses.len();
        game.act(&others[0], GameAction::Guess(secret), 0).unwrap();
        assert_eq!(game.guesses.len(), before);
        assert_eq!(game.phase, Phase::Drawing); // others[1] still guessing
    }

    #[test]
    fn picks_a_unique_random_word_each_round() {
        let mut game = started(&["alice", "bob"]);
        assert!(game.words.len() > 50); // a big bank, not the old handful
        let mut seen = std::collections::HashSet::new();
        seen.insert(game.secret.unwrap());
        for r in 1..8 {
            game.begin_round(r, 0);
            choose_first(&mut game);
            assert!(
                seen.insert(game.secret.unwrap()),
                "word repeated within a game"
            );
        }
    }

    #[test]
    fn word_banks_are_non_empty_and_have_no_duplicates() {
        for difficulty in [
            Difficulty::Easy,
            Difficulty::Normal,
            Difficulty::Hard,
            Difficulty::VeryHard,
        ] {
            let bank = word_bank(difficulty);
            // A non-empty bank is load-bearing: pick_choices does `% words.len()`.
            assert!(!bank.is_empty(), "{difficulty:?} bank is empty");
            let unique: std::collections::HashSet<_> = bank.iter().collect();
            assert_eq!(
                unique.len(),
                bank.len(),
                "{difficulty:?} bank has duplicates"
            );
        }
    }

    fn at_choosing(humans: &[&str]) -> HumanPictionary {
        let mut game = HumanPictionary::new(
            "room1".into(),
            &id(humans[0]),
            RoomConfig::default(),
            guesser(),
        );
        for h in humans {
            game.join(&id(h)).unwrap();
        }
        game.act(humans[0], GameAction::Start, 0).unwrap();
        game
    }

    #[test]
    fn drawer_picks_word_from_a_private_shortlist() {
        let mut game = at_choosing(&["alice", "bob"]);
        assert_eq!(game.phase, Phase::Choosing);
        let drawer = game.drawer_id.clone().unwrap();
        let other = if drawer == "alice" { "bob" } else { "alice" };
        // The shortlist is the drawer's alone and never rides in the broadcast.
        assert_eq!(game.choices_for(&drawer).len(), WORD_CHOICES);
        assert!(game.choices_for(other).is_empty());
        assert!(game.secret.is_none());
        match game.view().game {
            GameView::HumanPictionary { secret, .. } => assert!(secret.is_none()),
            _ => panic!("wrong view"),
        }
        // A non-drawer can't choose; an off-list word is rejected.
        assert_eq!(
            game.act(other, GameAction::Choose(game.choices[0].to_string()), 0),
            Err(GameError::NotDrawer)
        );
        assert_eq!(
            game.act(&drawer, GameAction::Choose("never-offered".into()), 0),
            Err(GameError::UnknownTarget)
        );
        // Committing an offered word opens the Drawing phase with that secret.
        let pick = game.choices[2];
        game.act(&drawer, GameAction::Choose(pick.to_string()), 0)
            .unwrap();
        assert_eq!(game.phase, Phase::Drawing);
        assert_eq!(game.secret, Some(pick));
        assert_eq!(game.word_for(&drawer), Some(pick));
        assert!(game.choices_for(&drawer).is_empty());
    }

    #[test]
    fn choosing_times_out_to_an_auto_pick() {
        let mut game = at_choosing(&["alice", "bob"]);
        let offered = game.choices.clone();
        // The deadline is choose_secs; advancing past it picks the first word.
        assert!(game.advance(game.config.choose_secs));
        assert_eq!(game.phase, Phase::Drawing);
        assert_eq!(game.secret, Some(offered[0]));
    }

    #[test]
    fn drawer_leaving_while_choosing_skips_the_round() {
        let mut game = at_choosing(&["alice", "bob"]);
        let drawer = game.drawer_id.clone().unwrap();
        game.set_connected(&drawer, false);
        game.nudge(0);
        assert_eq!(game.phase, Phase::Reveal);
        match game.view().game {
            GameView::HumanPictionary { unavailable, .. } => assert!(unavailable),
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn a_guesser_scores_at_most_once_per_round() {
        // Three humans (plus the AI) so the round stays live after the first solve.
        let mut game = started(&["alice", "bob", "carol"]);
        let secret = game.secret.unwrap().to_string();
        let others = non_drawers(&game);
        game.act(&others[0], GameAction::Guess(secret.clone()), 0)
            .unwrap();
        assert_eq!(game.roster.find(&others[0]).unwrap().score, 5);
        assert_eq!(game.phase, Phase::Drawing); // others[1] still owes a guess
                                                // Re-typing the word is a silent no-op — no extra points, no extra lines.
        let feed = game.guesses.len();
        for _ in 0..5 {
            game.act(&others[0], GameAction::Guess(secret.clone()), 0)
                .unwrap();
        }
        assert_eq!(game.roster.find(&others[0]).unwrap().score, 5);
        assert_eq!(game.guesses.len(), feed);
        // A *different* guesser still scores once (rank 1 → +4); the guard is
        // per-player, not a global "already solved" latch.
        game.act(&others[1], GameAction::Guess(secret), 0).unwrap();
        assert_eq!(game.roster.find(&others[1]).unwrap().score, 4);
    }

    #[test]
    fn out_of_order_and_stale_round_uploads_keep_the_newest_drawing() {
        let mut game = started(&["alice", "bob"]);
        let drawer = game.drawer_id.clone().unwrap();
        let round = game.round;
        let art = |g: &HumanPictionary| g.art().map(|a| a.bytes.to_vec());
        game.apply_drawing(&drawer, b"three".to_vec(), Some(round), Some(3), 10)
            .unwrap();
        assert_eq!(
            (game.display_snapshot, art(&game)),
            (3, Some(b"three".to_vec()))
        );
        // An out-of-order earlier seq is dropped — the newer image stands.
        game.apply_drawing(&drawer, b"two".to_vec(), Some(round), Some(2), 11)
            .unwrap();
        assert_eq!(
            (game.display_snapshot, art(&game)),
            (3, Some(b"three".to_vec()))
        );
        // A straggler tagged with another round is dropped too.
        game.apply_drawing(&drawer, b"old".to_vec(), Some(round + 9), Some(99), 12)
            .unwrap();
        assert_eq!(
            (game.display_snapshot, art(&game)),
            (3, Some(b"three".to_vec()))
        );
        // A forward seq advances the snapshot.
        game.apply_drawing(&drawer, b"four".to_vec(), Some(round), Some(4), 13)
            .unwrap();
        assert_eq!(
            (game.display_snapshot, art(&game)),
            (4, Some(b"four".to_vec()))
        );
    }
}
