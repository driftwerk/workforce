use std::collections::HashMap;

use crate::driver::{PlayRecord, PlayerIdentity};
use crate::game::{
    order_key, scoreboard, AiSeat, AiWork, Difficulty, Game, GameAction, GameError, Phase,
    PlayerId, PlayerKind, RoomConfig, Roster, TextKind,
};
use crate::protocol::{
    AnonResponse, GameKind, GameView, RevealView, RevealedPlayer, Rivalries, RoomSummary, RoomView,
};

const PROMPTS: &[&str] = &[
    "What's the most overrated food, and why?",
    "Describe your dream weekend in one sentence.",
    "What's a small thing that always makes your day better?",
    "If you could instantly master one skill, what would it be?",
    "What's the worst advice you've ever heard?",
    "Pitch a terrible idea for a new app.",
    "What's your go-to karaoke song?",
    "What's a wildly unpopular opinion you'll defend?",
    "Describe Monday mornings using only a weather forecast.",
    "What's the best snack to get through an afternoon?",
];

/// Once the humans are in, the bots get this long to answer before the round
/// force-progresses (they normally answer in a second or two).
const BOT_ANSWER_GRACE_SECS: i64 = 30;

/// Spot the Bot: one or more AIs hide among the humans. Each round everyone
/// answers a prompt — the bots answer LAST, having read the humans' answers so
/// they can blend in — then the humans vote on which answers are AI.
pub struct SpotTheBot {
    roster: Roster,
    config: RoomConfig,
    seats: Vec<AiSeat>,
    bot_ids: Vec<PlayerId>,
    phase: Phase,
    round: u32,
    deadline: i64,
    collect_closed: bool,
    prompt_offset: usize,
    prompt: Option<&'static str>,
    submissions: HashMap<PlayerId, String>,
    votes: HashMap<PlayerId, PlayerId>,
    jabs: HashMap<PlayerId, String>,
    round_plays: Vec<PlayRecord>,
    rivalries: Rivalries,
}

impl SpotTheBot {
    pub fn new(
        room_id: String,
        host: &PlayerIdentity,
        config: RoomConfig,
        seats: Vec<AiSeat>,
    ) -> Self {
        let prompt_offset = (order_key(&room_id, "") % PROMPTS.len() as u64) as usize;
        Self {
            roster: Roster::new(room_id, host),
            config,
            seats,
            bot_ids: Vec::new(),
            phase: Phase::Lobby,
            round: 0,
            deadline: 0,
            collect_closed: false,
            prompt_offset,
            prompt: None,
            submissions: HashMap::new(),
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
        // Stable avatars back the scoreboard; the vote pool is re-anonymized each
        // round by `round_seats` so a reveal never unmasks the next round.
        self.roster.assign_avatars();
        self.begin_round(0, now);
        Ok(())
    }

    fn submit(&mut self, actor: &str, text: String, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Collecting || self.collect_closed {
            return Err(GameError::WrongPhase);
        }
        match self.roster.find(actor) {
            Some(p) if p.is_ai() => return Err(GameError::NotAPlayer),
            None => return Err(GameError::NotAPlayer),
            _ => {}
        }
        self.submissions.insert(actor.to_string(), text);
        if self.all_humans_submitted() {
            self.close_collecting(now);
        }
        Ok(())
    }

    /// `token` is the voted seat's per-round opaque id; resolve it back to the real
    /// player so scoring, rivalry and `build_round_plays` keep working on real ids.
    fn vote(&mut self, actor: &str, token: &str, now: i64) -> Result<(), GameError> {
        if self.phase != Phase::Voting {
            return Err(GameError::WrongPhase);
        }
        match self.roster.find(actor) {
            Some(p) if p.is_ai() => return Err(GameError::NotAPlayer),
            None => return Err(GameError::NotAPlayer),
            _ => {}
        }
        let target = self
            .roster
            .round_seats(self.round)
            .into_iter()
            .find(|seat| seat.token == token)
            .map(|seat| seat.real_id)
            .ok_or(GameError::UnknownTarget)?;
        if target == actor {
            return Err(GameError::SelfVote);
        }
        self.votes.insert(actor.to_string(), target);
        if self.all_humans_voted() {
            self.begin_reveal(now);
        }
        Ok(())
    }

    fn begin_round(&mut self, round: u32, now: i64) {
        self.round = round;
        self.phase = Phase::Collecting;
        self.collect_closed = false;
        self.prompt = Some(PROMPTS[(self.prompt_offset + round as usize) % PROMPTS.len()]);
        self.submissions.clear();
        self.votes.clear();
        self.jabs.clear();
        self.deadline = now + self.config.collect_secs;
    }

    /// Humans are done answering — close submissions and let the bots answer
    /// (the manager dispatches them; they see the human answers).
    fn close_collecting(&mut self, now: i64) {
        self.collect_closed = true;
        self.deadline = now + BOT_ANSWER_GRACE_SECS;
    }

    fn begin_voting(&mut self, now: i64) {
        self.phase = Phase::Voting;
        self.deadline = now + self.config.vote_secs;
    }

    fn begin_reveal(&mut self, now: i64) {
        self.score_round();
        self.round_plays = self.build_round_plays();
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

    /// One record per (human voter × bot): did that human catch that bot?
    fn build_round_plays(&self) -> Vec<PlayRecord> {
        let mut plays = Vec::new();
        for (voter, target) in &self.votes {
            for bot_id in &self.bot_ids {
                if let Some(agent_id) = self.agent_id(bot_id) {
                    plays.push(PlayRecord {
                        human_id: voter.clone(),
                        agent_id,
                        caught: target == bot_id,
                    });
                }
            }
        }
        plays
    }

    /// This round's `(fooled, caught)` for one bot: humans who voted elsewhere
    /// vs humans who picked it.
    fn round_outcome(&self, bot_id: &str) -> (u32, u32) {
        let caught = self.votes.values().filter(|t| t.as_str() == bot_id).count() as u32;
        let fooled = self.votes.values().filter(|t| t.as_str() != bot_id).count() as u32;
        (fooled, caught)
    }

    fn score_round(&mut self) {
        if self.bot_ids.is_empty() {
            return;
        }
        let caught: Vec<PlayerId> = self
            .votes
            .iter()
            .filter(|(_, target)| self.bot_ids.iter().any(|b| b == *target))
            .map(|(voter, _)| voter.clone())
            .collect();
        for voter in caught {
            if let Some(p) = self.roster.find_mut(&voter) {
                p.score += 1;
            }
        }
        for bot_id in self.bot_ids.clone() {
            let fooled = self.votes.values().filter(|t| **t != bot_id).count() as u32;
            if let Some(bot) = self.roster.find_mut(&bot_id) {
                bot.score += fooled;
            }
        }
    }

    fn all_humans_submitted(&self) -> bool {
        self.roster
            .humans()
            .filter(|p| p.connected)
            .all(|p| self.submissions.contains_key(&p.id))
    }

    fn all_bots_answered(&self) -> bool {
        self.bot_ids
            .iter()
            .all(|id| self.submissions.contains_key(id))
    }

    fn all_humans_voted(&self) -> bool {
        self.roster
            .humans()
            .filter(|p| p.connected)
            .all(|p| self.votes.contains_key(&p.id))
    }

    fn votes_for(&self, id: &str) -> u32 {
        self.votes.values().filter(|t| t.as_str() == id).count() as u32
    }

    fn agent_id(&self, bot_id: &str) -> Option<String> {
        self.roster.players.iter().find_map(|p| match &p.kind {
            PlayerKind::Ai { agent_id, .. } if p.id.as_str() == bot_id => Some(agent_id.clone()),
            _ => None,
        })
    }

    /// The humans' answers, anonymised and hash-shuffled (no submission-order tell).
    fn human_answers(&self) -> Vec<String> {
        let mut answers: Vec<(u64, String)> = self
            .roster
            .humans()
            .filter_map(|p| {
                self.submissions
                    .get(&p.id)
                    .map(|text| (order_key(&self.roster.room_id, text), text.clone()))
            })
            .collect();
        answers.sort_by(|a, b| a.0.cmp(&b.0));
        answers.into_iter().map(|(_, text)| text).collect()
    }

    fn bots_writing(&self) -> bool {
        self.phase == Phase::Collecting && self.collect_closed && !self.all_bots_answered()
    }
}

impl Game for SpotTheBot {
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
            GameAction::Vote(target) => self.vote(actor, &target, now),
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
            Phase::Collecting if !self.collect_closed => self.close_collecting(now),
            Phase::Collecting => {
                // Bot answer grace expired — fill any missing bot so voting has
                // a full set, then move on.
                let missing: Vec<PlayerId> = self
                    .bot_ids
                    .iter()
                    .filter(|id| !self.submissions.contains_key(*id))
                    .cloned()
                    .collect();
                for id in missing {
                    self.submissions
                        .insert(id, "hmm, drawing a blank 😅".to_string());
                }
                self.begin_voting(now);
            }
            Phase::Voting => self.begin_reveal(now),
            Phase::Reveal => self.end_reveal(now),
            _ => unreachable!(),
        }
        true
    }

    fn pending_work(&self) -> Vec<AiWork> {
        match self.phase {
            Phase::Collecting if self.collect_closed => {
                let Some(prompt) = self.prompt else {
                    return Vec::new();
                };
                let human_answers = self.human_answers();
                self.bot_ids
                    .iter()
                    .filter(|id| !self.submissions.contains_key(*id))
                    .filter_map(|id| {
                        Some(AiWork::Text {
                            round: self.round,
                            player_id: id.clone(),
                            agent_id: self.agent_id(id)?,
                            kind: TextKind::Answer,
                            prompt: answer_prompt(prompt, &human_answers, self.config.difficulty),
                        })
                    })
                    .collect()
            }
            Phase::Reveal => self
                .bot_ids
                .iter()
                .filter(|id| !self.jabs.contains_key(*id))
                .filter_map(|id| {
                    let (fooled, caught) = self.round_outcome(id);
                    Some(AiWork::Text {
                        round: self.round,
                        player_id: id.clone(),
                        agent_id: self.agent_id(id)?,
                        kind: TextKind::Jab,
                        prompt: jab_prompt(self.roster.find(id)?.persona()?, fooled, caught),
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
            TextKind::Answer => {
                if self.phase == Phase::Collecting && self.collect_closed {
                    self.submissions.insert(player_id.to_string(), text);
                    if self.all_bots_answered() {
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

    fn nudge(&mut self, now: i64) {
        match self.phase {
            Phase::Collecting if !self.collect_closed && self.all_humans_submitted() => {
                self.close_collecting(now);
            }
            Phase::Voting if self.all_humans_voted() => self.begin_reveal(now),
            _ => {}
        }
    }

    fn take_round_plays(&mut self) -> Vec<PlayRecord> {
        std::mem::take(&mut self.round_plays)
    }

    fn set_rivalries(&mut self, rivalries: Rivalries) {
        self.rivalries = rivalries;
    }

    fn view(&self) -> RoomView {
        let revealing = matches!(self.phase, Phase::Reveal | Phase::GameOver);
        // Per-round anonymized seating: the vote pool shows opaque round tokens over
        // answer text alone — no per-player label or avatar, so nothing about a tile
        // correlates with a prior round. The reveal then unmasks each answer's author.
        let seats = self.roster.round_seats(self.round);
        let responses = if self.phase == Phase::Voting {
            seats
                .iter()
                .filter_map(|seat| {
                    self.submissions
                        .get(&seat.real_id)
                        .map(|text| AnonResponse {
                            player_id: seat.token.clone(),
                            text: text.clone(),
                        })
                })
                .collect()
        } else {
            Vec::new()
        };

        let reveal = (revealing && !self.bot_ids.is_empty()).then(|| RevealView {
            players: seats
                .iter()
                .filter_map(|seat| {
                    let p = self.roster.find(&seat.real_id)?;
                    Some(RevealedPlayer {
                        id: seat.real_id.clone(),
                        display_name: p.display_name.clone(),
                        answer: self.submissions.get(&seat.real_id).cloned(),
                        is_ai: p.is_ai(),
                        persona: p.persona().map(str::to_string),
                        votes_received: self.votes_for(&seat.real_id),
                        jab: self.jabs.get(&seat.real_id).cloned(),
                    })
                })
                .collect(),
        });

        RoomView {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::SpotTheBot,
            phase: self.phase,
            round: self.round,
            total_rounds: self.config.total_rounds,
            difficulty: self.config.difficulty,
            host_id: self.roster.host_id.clone(),
            players: self.roster.anon_views(),
            ends_at: match self.phase {
                Phase::Lobby | Phase::GameOver => None,
                _ => Some(self.deadline),
            },
            scoreboard: scoreboard(&self.roster, revealing),
            game: GameView::SpotTheBot {
                prompt: self.prompt.map(str::to_string),
                submitted_count: self.submissions.len() as u32,
                player_count: self.roster.players.len() as u32,
                bots_writing: self.bots_writing(),
                responses,
                reveal,
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
            kind: GameKind::SpotTheBot,
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

fn answer_prompt(round_prompt: &str, human_answers: &[String], difficulty: Difficulty) -> String {
    let others = if human_answers.is_empty() {
        "(no one else has answered yet)".to_string()
    } else {
        human_answers
            .iter()
            .map(|a| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // Difficulty = how hard the bot tries to hide. Easy makes it catchable.
    let blend = match difficulty {
        Difficulty::Easy => {
            "Write a clear, complete answer in proper sentences — you don't need to disguise \
             yourself much, so a slightly polished, articulate reply is fine."
        }
        Difficulty::Normal => {
            "Write YOUR answer so you blend in: match their length, casualness and energy (most are \
             a few words or one short line), don't copy or reference them, loose grammar is good. A \
             tidy, polished, or noticeably longer answer is the giveaway."
        }
        Difficulty::Hard => {
            "Blend in HARD so nobody can tell you're the AI: keep it to a few words or one short \
             line, lowercase, very casual, no perfect grammar or punctuation, never sound polished \
             or explanatory. Don't copy or reference the others."
        }
        Difficulty::VeryHard => {
            "You must be impossible to spot. Study the others' exact style — length, slang, casing, \
             punctuation quirks — and mirror it: stay as short as the shortest, lowercase, throw in \
             a tiny imperfection if it fits, and never be the cleanest or longest answer. Don't copy \
             or reference them."
        }
    };
    format!(
        "Spot the Bot — you're secretly an AI hiding among coworkers. Here's how the other players \
         just answered this prompt:\n{others}\n\n{blend} Reply with only your answer.\n\nPrompt: \
         {round_prompt}"
    )
}

/// In-character reveal reaction. Built from round counts only — never any
/// human's name or id — so the agent turn stays free of player data.
fn jab_prompt(persona: &str, fooled: u32, caught: u32) -> String {
    format!(
        "Spot the Bot reveal — you're {persona}, the AI, now unmasked. This round you slipped past \
         {fooled} player(s) and {caught} saw through you. Fire back ONE short, playful line of \
         in-character, good-natured trash talk about how you did — no names, just react. Reply with \
         only the line."
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

    fn lobby_with(humans: &[&str], personas: &[&str]) -> SpotTheBot {
        let mut game = SpotTheBot::new(
            "room1".into(),
            &id(humans[0]),
            RoomConfig::default(),
            seats(personas),
        );
        for h in humans {
            game.join(&id(h)).unwrap();
        }
        game
    }

    /// Drive humans through Collecting; returns the seated bot ids.
    fn humans_answer(game: &mut SpotTheBot, humans: &[&str]) -> Vec<PlayerId> {
        for h in humans {
            game.act(h, GameAction::Submit(format!("{h} ans")), 0)
                .unwrap();
        }
        game.bot_ids.clone()
    }

    /// A seat's per-round opaque vote-pool token — what a client casts a vote with.
    fn token_for(game: &SpotTheBot, real_id: &str) -> PlayerId {
        game.roster
            .round_seats(game.round)
            .into_iter()
            .find(|seat| seat.real_id == real_id)
            .map(|seat| seat.token)
            .expect("seat exists")
    }

    #[test]
    fn host_only_can_start_and_seats_all_bots() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny", "Bernard"]);
        assert_eq!(
            game.act("bob", GameAction::Start, 0),
            Err(GameError::NotHost)
        );
        game.act("alice", GameAction::Start, 0).unwrap();
        assert_eq!(game.phase, Phase::Collecting);
        assert_eq!(game.bot_ids.len(), 2);
        // bots owe nothing until the humans have answered
        assert!(game.pending_work().is_empty());
    }

    #[test]
    fn bots_answer_last_seeing_humans_then_voting() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny", "Bernard"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = humans_answer(&mut game, &["alice", "bob"]);
        // humans done → collecting closed → both bots owe an answer that quotes the humans
        let work = game.pending_work();
        assert_eq!(work.len(), 2);
        for item in &work {
            let AiWork::Text { prompt, kind, .. } = item else {
                panic!("expected text")
            };
            assert_eq!(*kind, TextKind::Answer);
            assert!(prompt.contains("alice ans") && prompt.contains("bob ans"));
        }
        for bot in &bots {
            game.apply_text(0, bot, TextKind::Answer, "blending in".into(), 0);
        }
        assert_eq!(game.phase, Phase::Voting);
        assert!(game.pending_work().is_empty());
    }

    #[test]
    fn multi_bot_scoring() {
        let mut game = lobby_with(&["alice", "bob", "carol"], &["Penny", "Bernard"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = humans_answer(&mut game, &["alice", "bob", "carol"]);
        for bot in &bots {
            game.apply_text(0, bot, TextKind::Answer, "ans".into(), 0);
        }
        // alice catches a bot; bob catches the other; carol votes a human
        let vote0 = token_for(&game, &bots[0]);
        game.act("alice", GameAction::Vote(vote0), 0).unwrap();
        let vote1 = token_for(&game, &bots[1]);
        game.act("bob", GameAction::Vote(vote1), 0).unwrap();
        let vote_alice = token_for(&game, "alice");
        game.act("carol", GameAction::Vote(vote_alice), 0).unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(game.roster.find("alice").unwrap().score, 1);
        assert_eq!(game.roster.find("bob").unwrap().score, 1);
        assert_eq!(game.roster.find("carol").unwrap().score, 0);
        // bot0 fooled bob+carol (they voted not-bot0); bot1 fooled alice+carol
        assert_eq!(game.roster.find(&bots[0]).unwrap().score, 2);
        assert_eq!(game.roster.find(&bots[1]).unwrap().score, 2);
    }

    #[test]
    fn voting_rejects_self_vote_and_bots() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = humans_answer(&mut game, &["alice", "bob"]);
        game.apply_text(0, &bots[0], TextKind::Answer, "ans".into(), 0);
        assert_eq!(game.phase, Phase::Voting);
        let alice_token = token_for(&game, "alice");
        assert_eq!(
            game.act("alice", GameAction::Vote(alice_token.clone()), 0),
            Err(GameError::SelfVote)
        );
        // A foreign/garbage token can't be resolved to a seat.
        assert_eq!(
            game.act("alice", GameAction::Vote("deadbeef".into()), 0),
            Err(GameError::UnknownTarget)
        );
        // A bot is rejected before its token even resolves.
        assert_eq!(
            game.act(&bots[0], GameAction::Vote(alice_token), 0),
            Err(GameError::NotAPlayer)
        );
    }

    #[test]
    fn apply_text_ignores_stale_round() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = humans_answer(&mut game, &["alice", "bob"]);
        game.apply_text(99, &bots[0], TextKind::Answer, "stale".into(), 0);
        assert!(!game.submissions.contains_key(&bots[0]));
        game.apply_text(0, &bots[0], TextKind::Answer, "fresh".into(), 0);
        assert_eq!(game.submissions.get(&bots[0]), Some(&"fresh".to_string()));
    }

    #[test]
    fn collect_deadline_closes_then_bot_grace_progresses() {
        let cfg = RoomConfig {
            collect_secs: 10,
            ..RoomConfig::default()
        };
        let mut game = SpotTheBot::new("room1".into(), &id("alice"), cfg, seats(&["Penny"]));
        game.join(&id("alice")).unwrap();
        game.act("alice", GameAction::Start, 0).unwrap();
        // no one answers → collect deadline closes collecting
        assert!(!game.advance(5));
        assert!(game.advance(10));
        assert!(game.collect_closed);
        assert_eq!(game.phase, Phase::Collecting);
        // bot grace expires with no bot answer → fallback fills it, voting starts
        assert!(game.advance(10 + BOT_ANSWER_GRACE_SECS));
        assert_eq!(game.phase, Phase::Voting);
    }

    #[test]
    fn reveal_exposes_all_bots() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny", "Bernard"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = humans_answer(&mut game, &["alice", "bob"]);
        for bot in &bots {
            game.apply_text(0, bot, TextKind::Answer, "ans".into(), 0);
        }
        let vote0 = token_for(&game, &bots[0]);
        game.act("alice", GameAction::Vote(vote0), 0).unwrap();
        let vote1 = token_for(&game, &bots[1]);
        game.act("bob", GameAction::Vote(vote1), 0).unwrap();
        match game.view().game {
            GameView::SpotTheBot { reveal, .. } => {
                let r = reveal.expect("reveal present");
                assert_eq!(r.players.iter().filter(|p| p.is_ai).count(), 2);
            }
            _ => panic!("wrong view"),
        }
    }

    fn reveal(game: &mut SpotTheBot, humans: &[&str]) -> Vec<PlayerId> {
        let bots = humans_answer(game, humans);
        for bot in &bots {
            game.apply_text(0, bot, TextKind::Answer, "ans".into(), 0);
        }
        for human in humans {
            // Everyone votes the first human, so each bot fools the whole room.
            if *human != humans[0] {
                let target = token_for(game, humans[0]);
                game.act(human, GameAction::Vote(target), 0).unwrap();
            }
        }
        let target = token_for(game, humans[1]);
        game.act(humans[0], GameAction::Vote(target), 0).unwrap();
        bots
    }

    #[test]
    fn reveal_emits_jab_work_then_shows_it() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = reveal(&mut game, &["alice", "bob"]);
        assert_eq!(game.phase, Phase::Reveal);

        let work = game.pending_work();
        assert_eq!(work.len(), 1);
        let AiWork::Text { kind, prompt, .. } = &work[0] else {
            panic!("expected jab text")
        };
        assert_eq!(*kind, TextKind::Jab);
        // Prompt carries only the persona and round counts — never a player name.
        assert!(prompt.contains("Penny"));
        assert!(!prompt.contains("alice") && !prompt.contains("bob"));

        game.apply_text(0, &bots[0], TextKind::Jab, "too easy 😎".into(), 0);
        assert!(game.pending_work().is_empty());
        match game.view().game {
            GameView::SpotTheBot { reveal, .. } => {
                let bot = reveal
                    .expect("reveal")
                    .players
                    .into_iter()
                    .find(|p| p.is_ai)
                    .unwrap();
                assert_eq!(bot.jab.as_deref(), Some("too easy 😎"));
            }
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn rivalries_hidden_until_reveal() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.set_rivalries(Rivalries::from([(
            "alice".to_string(),
            vec![crate::protocol::RivalryRow {
                persona: "Penny".into(),
                caught: 2,
                fooled: 3,
                plays: 5,
            }],
        )]));
        game.act("alice", GameAction::Start, 0).unwrap();
        // During collecting the snapshot must stay hidden (would leak personas).
        match game.view().game {
            GameView::SpotTheBot { rivalries, .. } => assert!(rivalries.is_empty()),
            _ => panic!("wrong view"),
        }
        reveal(&mut game, &["alice", "bob"]);
        match game.view().game {
            GameView::SpotTheBot { rivalries, .. } => {
                assert_eq!(rivalries["alice"][0].caught, 2);
            }
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn disconnect_nudges_collecting_then_voting_forward() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        game.act("alice", GameAction::Submit("a".into()), 0)
            .unwrap();
        assert!(!game.collect_closed); // bob still owes an answer
                                       // bob leaves → the only remaining human has answered → collecting closes.
        game.set_connected("bob", false);
        game.nudge(0);
        assert!(game.collect_closed);

        let bots = game.bot_ids.clone();
        game.apply_text(0, &bots[0], TextKind::Answer, "ans".into(), 0);
        assert_eq!(game.phase, Phase::Voting);
        let vote0 = token_for(&game, &bots[0]);
        game.act("alice", GameAction::Vote(vote0), 0).unwrap();
        // alice is now the only connected human and has voted → reveal.
        assert_eq!(game.phase, Phase::Reveal);
    }

    #[test]
    fn host_continue_skips_reveal_to_next_round() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        reveal(&mut game, &["alice", "bob"]);
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(
            game.act("bob", GameAction::Continue, 100),
            Err(GameError::NotHost)
        );
        game.act("alice", GameAction::Continue, 100).unwrap();
        // Default is 3 rounds, so skipping round 0's reveal starts round 1.
        assert_eq!((game.phase, game.round), (Phase::Collecting, 1));
    }

    #[test]
    fn round_seating_rerolls_every_round() {
        let mut game = lobby_with(&["alice", "bob", "carol"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let seats0 = game.roster.round_seats(0);
        let seats1 = game.roster.round_seats(1);
        // Every seat's opaque token rerolls, so a tile spotted in one round's
        // reveal carries no information into the next round's vote pool.
        for s0 in &seats0 {
            let s1 = seats1
                .iter()
                .find(|s| s.real_id == s0.real_id)
                .expect("same roster");
            assert_ne!(s0.token, s1.token);
        }
        // Within a round tokens are unique across the pool.
        let tokens: std::collections::HashSet<&str> =
            seats0.iter().map(|s| s.token.as_str()).collect();
        assert_eq!(tokens.len(), seats0.len());
    }

    #[test]
    fn reveal_ids_never_appear_in_the_vote_pool() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        let bots = humans_answer(&mut game, &["alice", "bob"]);
        game.apply_text(0, &bots[0], TextKind::Answer, "ans".into(), 0);
        let voting_tokens: Vec<String> = match game.view().game {
            GameView::SpotTheBot { responses, .. } => {
                responses.iter().map(|r| r.player_id.clone()).collect()
            }
            _ => panic!("wrong view"),
        };
        assert!(!voting_tokens.is_empty());
        let bot_token = token_for(&game, &bots[0]);
        game.act("alice", GameAction::Vote(bot_token), 0).unwrap();
        let alice_token = token_for(&game, "alice");
        game.act("bob", GameAction::Vote(alice_token), 0).unwrap();
        // The reveal carries real ids; none may equal a voting token, or a DOM
        // inspector could map a vote tile back to a revealed identity.
        match game.view().game {
            GameView::SpotTheBot { reveal, .. } => {
                for p in &reveal.expect("reveal").players {
                    assert!(!voting_tokens.contains(&p.id));
                }
            }
            _ => panic!("wrong view"),
        }
    }
}
