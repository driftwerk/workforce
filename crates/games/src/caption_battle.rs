use std::collections::HashMap;

use crate::driver::PlayerIdentity;
use crate::game::{
    order_key, scoreboard, AiSeat, AiWork, Difficulty, Game, GameAction, GameError, Phase,
    PlayerId, PlayerKind, RoomConfig, Roster, TextKind,
};
use crate::protocol::{
    CaptionReveal, DefOptionView, GameKind, GameView, RevealedCaption, RoomSummary, RoomView,
};

const PROMPTS: &[&str] = &[
    "Pitch a terrible idea for a startup.",
    "Describe the worst possible superpower.",
    "Name a rejected ice-cream flavour.",
    "Invent a useless kitchen gadget.",
    "What's the worst thing to say on a first date?",
    "A rejected slogan for your home town.",
    "An honest tagline for Monday mornings.",
    "A terrible motivational poster caption.",
    "Name a racehorse destined to lose.",
    "Describe your least useful talent.",
    "The worst superhero sidekick.",
    "An honest one-line review of planet Earth.",
    "A disastrous idea for a new reality TV show.",
    "A terrible name for a cologne.",
    "The worst possible theme for a wedding.",
    "A new public holiday nobody asked for.",
];

/// In-genre filler if a bot's caption turn fails (overrides the generic one).
const CAPTION_FALLBACK: &str = "…I've got nothing, you win this one";

/// A caption in the vote pool. `id` is an opaque per-round hash so authorship
/// stays hidden until the reveal; `author` is kept server-side.
struct CaptionOption {
    id: String,
    author: PlayerId,
    text: String,
}

/// Caption Battle: everyone (humans + AI bots) writes a funny caption for the
/// round's prompt; the captions are shuffled anonymously and the room votes for
/// the funniest. You score for every vote your caption gets.
pub struct CaptionBattle {
    roster: Roster,
    config: RoomConfig,
    seats: Vec<AiSeat>,
    bot_ids: Vec<PlayerId>,
    phase: Phase,
    round: u32,
    deadline: i64,
    prompt_offset: usize,
    prompt: Option<&'static str>,
    submissions: HashMap<PlayerId, String>,
    options: Vec<CaptionOption>,
    votes: HashMap<PlayerId, String>,
    jabs: HashMap<PlayerId, String>,
}

impl CaptionBattle {
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
            prompt_offset,
            prompt: None,
            submissions: HashMap::new(),
            options: Vec::new(),
            votes: HashMap::new(),
            jabs: HashMap::new(),
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
        if option.author == actor {
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
        self.prompt = Some(PROMPTS[(self.prompt_offset + round as usize) % PROMPTS.len()]);
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

    /// Captions shuffled behind opaque per-round ids so authorship is hidden
    /// during voting (keys domain-separated + salted by round).
    fn build_options(&self) -> Vec<CaptionOption> {
        // audit G1: salt with the per-room SECRET (not the public room_id) so a
        // client can't compute which option is which author's caption.
        let salt = format!("{}:{}", self.roster.salt, self.round);
        let mut keyed: Vec<(u64, PlayerId, String)> = self
            .submissions
            .iter()
            .map(|(author, text)| {
                (
                    order_key(&salt, &format!("p:{author}")),
                    author.clone(),
                    text.clone(),
                )
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        keyed
            .into_iter()
            .map(|(key, author, text)| CaptionOption {
                id: format!("{key:016x}"),
                author,
                text,
            })
            .collect()
    }

    fn score_round(&mut self) {
        let mut awards: Vec<PlayerId> = Vec::new();
        for option_id in self.votes.values() {
            if let Some(option) = self.options.iter().find(|o| &o.id == option_id) {
                awards.push(option.author.clone());
            }
        }
        for author in awards {
            if let Some(player) = self.roster.find_mut(&author) {
                player.score += 1;
            }
        }
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

    fn option_for(&self, author: &str) -> Option<&CaptionOption> {
        self.options.iter().find(|o| o.author == author)
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

    fn build_reveal(&self) -> CaptionReveal {
        let top = self
            .options
            .iter()
            .map(|o| self.votes_for(&o.id))
            .max()
            .unwrap_or(0);
        let entries = self
            .options
            .iter()
            .map(|option| {
                let player = self.roster.find(&option.author);
                let votes_received = self.votes_for(&option.id);
                RevealedCaption {
                    text: option.text.clone(),
                    author_display_name: player.map(|p| p.display_name.clone()),
                    is_ai: player.map(|p| p.is_ai()).unwrap_or(false),
                    persona: player.and_then(|p| p.persona().map(str::to_string)),
                    votes_received,
                    is_winner: top > 0 && votes_received == top,
                    jab: self.jabs.get(&option.author).cloned(),
                }
            })
            .collect();
        CaptionReveal { entries }
    }
}

impl Game for CaptionBattle {
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
                let Some(prompt) = self.prompt else {
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
                            prompt: caption_prompt(prompt, self.config.difficulty),
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
                    let votes = self.votes_for(&option.id);
                    Some(AiWork::Text {
                        round: self.round,
                        player_id: id.clone(),
                        agent_id: self.agent_id(id)?,
                        kind: TextKind::Jab,
                        prompt: jab_prompt(self.roster.find(id)?.persona()?, votes),
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
        CAPTION_FALLBACK.to_string()
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
            kind: GameKind::CaptionBattle,
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
            game: GameView::CaptionBattle {
                prompt: self.prompt.unwrap_or_default().to_string(),
                submitted_count: self.submissions.len() as u32,
                player_count: self.roster.players.len() as u32,
                bots_writing: self.bots_writing(),
                options,
                reveal: revealing.then(|| self.build_reveal()),
            },
        }
    }

    fn summary(&self) -> RoomSummary {
        RoomSummary {
            room_id: self.roster.room_id.clone(),
            kind: GameKind::CaptionBattle,
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

fn caption_prompt(prompt: &str, difficulty: Difficulty) -> String {
    let wit = match difficulty {
        Difficulty::Easy => "Keep it light and a little obvious — don't try too hard.",
        Difficulty::Normal => "Write a short, genuinely funny one-liner.",
        Difficulty::Hard => "Write a sharp, clever, laugh-out-loud caption.",
        Difficulty::VeryHard => {
            "Write the single funniest, most unexpected caption you can — it should win the room."
        }
    };
    format!(
        "Caption Battle — answer this prompt with ONE short, funny caption (you're competing with the \
         humans for votes). {wit} No quotes, no preamble. Reply with only your caption.\n\nPrompt: \
         {prompt}"
    )
}

/// In-character reveal reaction. Counts only — no player names/ids.
fn jab_prompt(persona: &str, votes: u32) -> String {
    format!(
        "Caption Battle reveal — you're {persona}, the AI. Your caption got {votes} vote(s). Fire \
         back ONE short, playful, in-character line gloating or sulking about your score — no names. \
         Reply with only the line."
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

    fn lobby_with(humans: &[&str], personas: &[&str]) -> CaptionBattle {
        let mut game = CaptionBattle::new(
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

    fn to_voting(game: &mut CaptionBattle, humans: &[&str]) {
        for h in humans {
            game.act(h, GameAction::Submit(format!("{h}'s caption")), 0)
                .unwrap();
        }
        for (i, bot) in game.bot_ids.clone().iter().enumerate() {
            game.apply_text(0, bot, TextKind::Answer, format!("bot caption {i}"), 0);
        }
    }

    fn author_opt(game: &CaptionBattle, who: &str) -> String {
        game.option_for(who).unwrap().id.clone()
    }

    #[test]
    fn bots_caption_in_parallel_then_voting() {
        let mut game = lobby_with(&["alice", "bob"], &["Penny", "Bernard"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        assert_eq!(game.phase, Phase::Collecting);
        let work = game.pending_work();
        assert_eq!(work.len(), 2);
        for w in &work {
            let AiWork::Text { kind, prompt, .. } = w else {
                panic!()
            };
            assert_eq!(*kind, TextKind::Answer);
            assert!(prompt.contains(game.prompt.unwrap()));
        }
        to_voting(&mut game, &["alice", "bob"]);
        assert_eq!(game.phase, Phase::Voting);
        // 2 humans + 2 bots = 4 anonymized captions, opaque ids, no real answer
        match game.view().game {
            GameView::CaptionBattle { options, .. } => {
                assert_eq!(options.len(), 4);
                assert!(options.iter().all(|o| o.id.len() == 16));
            }
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn vote_rejects_self_and_scores_per_vote() {
        let mut game = lobby_with(&["alice", "bob", "carol"], &["Penny"]);
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob", "carol"]);
        assert_eq!(
            game.act("alice", GameAction::Vote(author_opt(&game, "alice")), 0),
            Err(GameError::SelfVote)
        );
        // bob + carol both vote alice's caption → alice +2
        game.act("bob", GameAction::Vote(author_opt(&game, "alice")), 0)
            .unwrap();
        game.act("carol", GameAction::Vote(author_opt(&game, "alice")), 0)
            .unwrap();
        game.act("alice", GameAction::Vote(author_opt(&game, "bob")), 0)
            .unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(game.roster.find("alice").unwrap().score, 2);
        assert_eq!(game.roster.find("bob").unwrap().score, 1);
        assert_eq!(game.roster.find("carol").unwrap().score, 0);
        match game.view().game {
            GameView::CaptionBattle { reveal, .. } => {
                let r = reveal.unwrap();
                let winner = r.entries.iter().find(|e| e.is_winner).unwrap();
                assert_eq!(winner.votes_received, 2);
            }
            _ => panic!("wrong view"),
        }
    }

    #[test]
    fn host_continue_skips_reveal_and_rejects_non_host() {
        let cfg = RoomConfig {
            total_rounds: 2,
            ..RoomConfig::default()
        };
        let mut game = CaptionBattle::new("room1".into(), &id("alice"), cfg, seats(&["Penny"]));
        game.join(&id("alice")).unwrap();
        game.join(&id("bob")).unwrap();
        game.act("alice", GameAction::Start, 0).unwrap();
        to_voting(&mut game, &["alice", "bob"]);
        game.act("alice", GameAction::Vote(author_opt(&game, "bob")), 0)
            .unwrap();
        game.act("bob", GameAction::Vote(author_opt(&game, "alice")), 0)
            .unwrap();
        assert_eq!(game.phase, Phase::Reveal);
        assert_eq!(
            game.act("bob", GameAction::Continue, 100),
            Err(GameError::NotHost)
        );
        game.act("alice", GameAction::Continue, 100).unwrap();
        assert_eq!((game.phase, game.round), (Phase::Collecting, 1));
    }
}
