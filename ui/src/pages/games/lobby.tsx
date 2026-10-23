import { useState, type ReactNode } from "react";
import { useNavigate } from "react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { gateway } from "@/lib/api-client";
import type { BalderdashPack, Difficulty, GameKind, GamesConfig, RoomSummary } from "./types";

interface GameMeta {
  kind: GameKind;
  emoji: string;
  name: string;
  blurb: string;
}

const GAMES: GameMeta[] = [
  {
    kind: "spot_the_bot",
    emoji: "🕵️",
    name: "Spot the Bot",
    blurb: "One or more teammates are secretly an AI — answer the prompts and vote out the machine.",
  },
  {
    kind: "reverse_pictionary",
    emoji: "🎨",
    name: "Reverse Pictionary",
    blurb: "The AI draws a secret word — race your team to guess it first.",
  },
  {
    kind: "balderdash",
    emoji: "📖",
    name: "Balderdash",
    blurb: "Bluff a fake answer — words, movie plots, acronyms or laws — then vote out the real one.",
  },
  {
    kind: "caption_battle",
    emoji: "📝",
    name: "Caption Battle",
    blurb: "Everyone writes the funniest caption for a silly prompt — the AI too — then vote the winner.",
  },
  {
    kind: "human_pictionary",
    emoji: "🖌️",
    name: "Human Pictionary",
    blurb: "You draw a secret word on a canvas — your team and the AI race to guess it.",
  },
];

const DIFFICULTIES: { value: Difficulty; label: string }[] = [
  { value: "easy", label: "Easy" },
  { value: "normal", label: "Normal" },
  { value: "hard", label: "Hard" },
  { value: "very-hard", label: "Very hard" },
];

const ROUND_OPTIONS = [3, 5, 7, 10];

const PACKS: { value: BalderdashPack; label: string }[] = [
  { value: "words", label: "Words" },
  { value: "movies", label: "Movies" },
  { value: "acronyms", label: "Acronyms" },
  { value: "laws", label: "Laws" },
];

interface CreateOpts {
  kind: GameKind;
  rounds: number;
  difficulty: Difficulty;
  bots?: number;
  pack?: BalderdashPack;
}

const PHASE_LABEL: Record<string, string> = {
  lobby: "In the lobby",
  collecting: "Writing answers",
  voting: "Voting",
  drawing: "Drawing",
  guessing: "Guessing",
  reveal: "Big reveal",
};

export default function GamesLobby() {
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const { data: rooms = [], isLoading, isError } = useQuery({
    queryKey: ["games-rooms"],
    queryFn: async () => {
      // When [games] is disabled the route is unmounted and the SPA fallback
      // returns index.html; treat any non-array body as "games unavailable".
      const data = await gateway.get<RoomSummary[]>("/games/api/rooms");
      if (!Array.isArray(data)) throw new Error("games-unavailable");
      return data;
    },
    retry: false,
    refetchInterval: 3000,
  });

  const { data: config } = useQuery({
    queryKey: ["games-config"],
    queryFn: () => gateway.get<GamesConfig>("/games/api/config"),
    retry: false,
  });
  const maxBots = Math.max(1, config?.max_bots ?? 1);

  const createRoom = useMutation({
    mutationFn: (opts: CreateOpts) =>
      gateway.post<{ room_id: string }>("/games/api/rooms", opts),
    onSuccess: ({ room_id }) => {
      queryClient.invalidateQueries({ queryKey: ["games-rooms"] });
      navigate(`/games/${room_id}`);
    },
  });

  return (
    <div className="space-y-8">
      <section className="g-card p-7 text-center sm:p-10">
        <div className="g-float text-6xl">🎉</div>
        <h1 className="g-display mt-3 text-4xl sm:text-5xl">Game time</h1>
        <p className="mx-auto mt-3 max-w-md text-lg font-semibold text-[var(--g-ink)]/70">
          Pick a game, share the room code, and play a round with your team — the AI joins in too.
        </p>
        {isError ? (
          <p className="mx-auto mt-6 max-w-md font-bold text-[var(--g-ink)]/70">
            🔌 Games aren't switched on for this workspace yet. An admin needs to set{" "}
            <code className="rounded bg-black/10 px-1 py-0.5">[games] enabled = true</code> and
            restart the hub.
          </p>
        ) : (
          <div className="mt-6 grid gap-4 sm:grid-cols-2">
            {GAMES.map((game) => (
              <GameCard
                key={game.kind}
                game={game}
                maxBots={maxBots}
                pending={createRoom.isPending}
                onCreate={createRoom.mutate}
              />
            ))}
          </div>
        )}
        {createRoom.isError && (
          <p className="mt-3 font-bold text-[var(--g-pop)]">
            Couldn't start a game — try again in a moment.
          </p>
        )}
      </section>

      {!isError && (
        <section className="space-y-3">
          <h2 className="g-display px-1 text-xl text-white">Open games</h2>
          {isLoading ? (
            <p className="g-chip">Loading…</p>
          ) : rooms.length === 0 ? (
            <div className="g-card p-6 text-center font-semibold text-[var(--g-ink)]/70">
              No games yet — start one above! 🎲
            </div>
          ) : (
            rooms.map((room) => {
              const game = GAMES.find((entry) => entry.kind === room.kind);
              return (
                <button
                  key={room.room_id}
                  className="g-card g-tile-pick flex w-full items-center gap-4 p-4 text-left"
                  onClick={() => navigate(`/games/${room.room_id}`)}
                >
                  <span className="grid h-12 w-12 place-items-center rounded-2xl bg-[var(--g-pop2)]/10 text-2xl">
                    {game?.emoji ?? "🎮"}
                  </span>
                  <div>
                    <div className="text-lg font-extrabold">
                      {game?.name ?? "Game"} · {room.room_id}
                    </div>
                    <div className="text-sm font-semibold text-[var(--g-ink)]/60">
                      Host {room.host || "—"} · {PHASE_LABEL[room.phase] ?? room.phase}
                    </div>
                  </div>
                  <span
                    className="ml-auto g-chip"
                    style={{ color: "#6d28d9", background: "#ede9fe", border: "none" }}
                  >
                    {room.player_count} 👥
                  </span>
                </button>
              );
            })
          )}
        </section>
      )}
    </div>
  );
}

function GameCard({
  game,
  maxBots,
  pending,
  onCreate,
}: {
  game: GameMeta;
  maxBots: number;
  pending: boolean;
  onCreate: (opts: CreateOpts) => void;
}) {
  const [difficulty, setDifficulty] = useState<Difficulty>("normal");
  const [rounds, setRounds] = useState(3);
  const [bots, setBots] = useState(1);
  const [pack, setPack] = useState<BalderdashPack>("words");
  const showBots =
    (game.kind === "spot_the_bot" ||
      game.kind === "balderdash" ||
      game.kind === "caption_battle") &&
    maxBots > 1;
  const showPack = game.kind === "balderdash";

  return (
    <div className="g-card p-5 text-left">
      <div className="text-4xl">{game.emoji}</div>
      <div className="mt-2 text-xl font-extrabold">{game.name}</div>
      <p className="mt-1 text-sm font-semibold text-[var(--g-ink)]/60">{game.blurb}</p>

      {showPack && (
        <PickerRow label="Pack">
          {PACKS.map((p) => (
            <Chip key={p.value} active={pack === p.value} onClick={() => setPack(p.value)}>
              {p.label}
            </Chip>
          ))}
        </PickerRow>
      )}

      <PickerRow label="Difficulty">
        {DIFFICULTIES.map((d) => (
          <Chip key={d.value} active={difficulty === d.value} onClick={() => setDifficulty(d.value)}>
            {d.label}
          </Chip>
        ))}
      </PickerRow>

      <PickerRow label="Rounds">
        {ROUND_OPTIONS.map((n) => (
          <Chip key={n} active={rounds === n} onClick={() => setRounds(n)}>
            {n}
          </Chip>
        ))}
      </PickerRow>

      {showBots && (
        <PickerRow label="🤖 Bots">
          {Array.from({ length: maxBots }, (_, i) => i + 1).map((n) => (
            <Chip key={n} active={bots === n} onClick={() => setBots(n)}>
              {n}
            </Chip>
          ))}
        </PickerRow>
      )}

      <button
        type="button"
        className="g-btn mt-4 w-full"
        onClick={() =>
          onCreate({
            kind: game.kind,
            rounds,
            difficulty,
            bots: showBots ? bots : undefined,
            pack: showPack ? pack : undefined,
          })
        }
        disabled={pending}
      >
        Start game ▶
      </button>
    </div>
  );
}

function PickerRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="mt-3 flex flex-wrap items-center gap-2">
      <span className="text-sm font-bold text-[var(--g-ink)]/70">{label}</span>
      {children}
    </div>
  );
}

function Chip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`g-tile px-3 py-1 text-sm font-extrabold ${active ? "g-tile-chosen" : "g-tile-pick"}`}
    >
      {children}
    </button>
  );
}
