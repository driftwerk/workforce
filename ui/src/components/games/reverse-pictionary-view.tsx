import { useEffect, useState } from "react";
import { useParams } from "react-router";
import { useGamesStore } from "@/stores/games-store";
import { useAuthStore } from "@/stores/auth-store";
import { LobbyShell } from "@/components/games/lobby-shell";
import { ScoresCard } from "@/components/games/scores-card";
import { GameOver } from "@/components/games/game-over";
import { HostContinue } from "@/components/games/host-continue";

export function ReversePictionaryView() {
  const view = useGamesStore((s) => s.view);
  const playerId = useGamesStore((s) => s.playerId);
  const start = useGamesStore((s) => s.start);
  const guess = useGamesStore((s) => s.guess);
  const token = useAuthStore((s) => s.token);
  const { roomId } = useParams();

  const [draft, setDraft] = useState("");
  const round = view?.round ?? -1;
  useEffect(() => {
    setDraft("");
  }, [round]);

  if (!view || view.game.view !== "reverse_pictionary") return null;
  const g = view.game;
  const artist = g.artist || "The AI";
  const isHost = view.host_id === playerId;
  const iGuessed = g.guessed.some((row) => row.player_id === playerId);
  const artUrl =
    g.art_round != null
      ? `/games/api/rooms/${roomId}/art?token=${encodeURIComponent(token ?? "")}&v=${g.art_round}`
      : null;

  function sendGuess() {
    if (draft.trim()) {
      guess(draft.trim());
      setDraft("");
    }
  }

  return (
    <>
      {view.phase === "lobby" && (
        <LobbyShell players={view.players} youId={playerId} isHost={isHost} onStart={start}>
          <span className="font-black text-[var(--g-pop2)]">{artist}</span> will draw a secret word
          and you race to guess it. Share code{" "}
          <span className="font-black text-[var(--g-pop2)]">{view.room_id}</span> with your team! 🎨
        </LobbyShell>
      )}

      {view.phase === "drawing" && (
        <section className="g-card p-10 text-center">
          <div className="g-pulse text-6xl">🎨</div>
          <h2 className="g-display mt-3 text-2xl">{artist} is drawing…</h2>
          <p className="mt-1 font-semibold text-[var(--g-ink)]/60">Get your guessing fingers ready ✏️</p>
        </section>
      )}

      {view.phase === "guessing" && (
        <section className="space-y-4">
          {artUrl && (
            <div className="g-card p-3">
              <img
                src={artUrl}
                alt="The AI's drawing"
                className="mx-auto w-full max-w-md rounded-2xl"
              />
            </div>
          )}
          {iGuessed ? (
            <div className="g-card p-5 text-center">
              <div className="g-float text-5xl">✅</div>
              <p className="mt-2 font-bold">You got it! Waiting for the others…</p>
            </div>
          ) : (
            <div className="g-card space-y-3 p-5">
              <p className="text-center text-lg font-extrabold text-[var(--g-pop2)]">What is it?</p>
              <input
                className="w-full rounded-2xl border-2 border-[var(--g-pop2)]/20 bg-white p-4 text-lg font-semibold outline-none focus:border-[var(--g-pop)]"
                maxLength={60}
                placeholder="Type your guess…"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    sendGuess();
                  }
                }}
              />
              <button className="g-btn w-full text-lg" onClick={sendGuess} disabled={!draft.trim()}>
                Guess! 🎯
              </button>
            </div>
          )}
          {g.guessed.length > 0 && (
            <div className="g-card p-4">
              <p className="mb-2 font-bold text-[var(--g-ink)]/60">Got it 🎉</p>
              <div className="flex flex-wrap gap-2">
                {g.guessed.map((row) => (
                  <span key={row.player_id} className="g-tile px-3 py-1 font-extrabold">
                    <span className="mr-1">{row.avatar}</span>
                    {row.label} <span className="text-[var(--g-pop2)]">#{row.rank}</span>
                  </span>
                ))}
              </div>
            </div>
          )}
        </section>
      )}

      {view.phase === "reveal" && (
        <section className="space-y-4">
          <div className="g-card p-6 text-center">
            {g.unavailable ? (
              <>
                <div className="text-5xl">🚫</div>
                <h2 className="g-display mt-2 text-2xl">{artist} couldn't draw this round</h2>
              </>
            ) : (
              <>
                <p className="font-bold text-[var(--g-ink)]/60">The word was</p>
                <h2 className="g-display text-3xl">{g.secret}</h2>
                {artUrl && (
                  <img
                    src={artUrl}
                    alt={g.secret ?? "drawing"}
                    className="mx-auto mt-3 w-full max-w-xs rounded-2xl"
                  />
                )}
              </>
            )}
          </div>
          <ScoresCard rows={view.scoreboard} youId={playerId} />
          <HostContinue isHost={isHost} round={view.round} totalRounds={view.total_rounds} />
        </section>
      )}

      {view.phase === "game_over" && <GameOver scoreboard={view.scoreboard} youId={playerId} />}
    </>
  );
}
