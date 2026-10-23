export type GameKind =
  | "spot_the_bot"
  | "reverse_pictionary"
  | "balderdash"
  | "caption_battle"
  | "human_pictionary";

export type Difficulty = "easy" | "normal" | "hard" | "very-hard";

export type BalderdashPack = "words" | "movies" | "acronyms" | "laws";

export type Phase =
  | "lobby"
  | "collecting"
  | "voting"
  | "choosing"
  | "drawing"
  | "guessing"
  | "reveal"
  | "game_over";

export interface PlayerView {
  id: string;
  label: string;
  avatar: string;
  connected: boolean;
}

export interface AnonResponse {
  player_id: string;
  text: string;
}

export interface RevealedPlayer {
  id: string;
  display_name: string;
  answer: string | null;
  is_ai: boolean;
  persona: string | null;
  votes_received: number;
  jab: string | null;
}

export interface RevealView {
  players: RevealedPlayer[];
}

export interface RivalryRow {
  persona: string;
  caught: number;
  fooled: number;
  plays: number;
}

export type Rivalries = Record<string, RivalryRow[]>;

export interface GuessRow {
  player_id: string;
  label: string;
  avatar: string;
  rank: number;
}

export interface ScoreRow {
  player_id: string;
  label: string;
  display_name: string;
  avatar: string;
  is_ai: boolean;
  score: number;
}

export type GameView =
  | {
      view: "spot_the_bot";
      prompt: string | null;
      submitted_count: number;
      player_count: number;
      bots_writing: boolean;
      responses: AnonResponse[];
      reveal: RevealView | null;
      rivalries: Rivalries;
    }
  | {
      view: "reverse_pictionary";
      artist: string;
      art_round: number | null;
      guessed: GuessRow[];
      secret: string | null;
      unavailable: boolean;
    }
  | {
      view: "balderdash";
      word: string;
      noun: string;
      submitted_count: number;
      player_count: number;
      bots_writing: boolean;
      options: DefOptionView[];
      reveal: BalderdashReveal | null;
      rivalries: Rivalries;
    }
  | {
      view: "caption_battle";
      prompt: string;
      submitted_count: number;
      player_count: number;
      bots_writing: boolean;
      options: DefOptionView[];
      reveal: CaptionReveal | null;
    }
  | {
      view: "human_pictionary";
      drawer_id: string | null;
      drawer_label: string | null;
      art_round: number | null;
      snapshot: number;
      guesses: GuessFeedRow[];
      correct: RoundGuessRow[];
      drawer_points: number;
      secret: string | null;
      unavailable: boolean;
    };

export interface GuessFeedRow {
  player_id: string;
  label: string;
  avatar: string;
  text: string;
  correct: boolean;
  is_ai: boolean;
  solved: boolean;
}

export interface RoundGuessRow {
  player_id: string;
  label: string;
  avatar: string;
  is_ai: boolean;
  points: number;
}

export interface CaptionReveal {
  entries: RevealedCaption[];
}

export interface RevealedCaption {
  text: string;
  author_display_name: string | null;
  is_ai: boolean;
  persona: string | null;
  votes_received: number;
  is_winner: boolean;
  jab: string | null;
}

export interface DefOptionView {
  id: string;
  text: string;
}

export interface BalderdashReveal {
  real_text: string;
  entries: RevealedDef[];
}

export interface RevealedDef {
  text: string;
  is_real: boolean;
  author_display_name: string | null;
  is_ai: boolean;
  persona: string | null;
  votes_received: number;
  jab: string | null;
}

export interface RoomView {
  room_id: string;
  kind: GameKind;
  phase: Phase;
  round: number;
  total_rounds: number;
  difficulty: Difficulty;
  host_id: string;
  players: PlayerView[];
  ends_at: number | null;
  scoreboard: ScoreRow[];
  game: GameView;
}

export interface RoomSummary {
  room_id: string;
  kind: GameKind;
  phase: Phase;
  player_count: number;
  host: string;
}

export interface GamesConfig {
  max_bots: number;
}

export type ServerGameMessage =
  | ({ type: "state" } & RoomView)
  | { type: "welcome"; player_id: string }
  | { type: "error"; message: string }
  | { type: "your_word"; word: string }
  | { type: "word_choices"; words: string[] }
  | { type: "pong" };
