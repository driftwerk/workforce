import { useEffect, useState } from "react";
import { useGamesStore } from "@/stores/games-store";
import { LobbyShell } from "@/components/games/lobby-shell";
import { ScoresCard } from "@/components/games/scores-card";
import { GameOver } from "@/components/games/game-over";
import { HostContinue } from "@/components/games/host-continue";
import type { RevealedPlayer, RivalryRow } from "@/pages/games/types";

export function SpotTheBotView() {
  const view = useGamesStore((s) => s.view);
  const playerId = useGamesStore((s) => s.playerId);
  const start = useGamesStore((s) => s.start);
  const submit = useGamesStore((s) => s.submit);
  const vote = useGamesStore((s) => s.vote);

  const [draft, setDraft] = useState("");
  const [actedKey, setActedKey] = useState("");
  const [votedTarget, setVotedTarget] = useState<string | null>(null);

  const round = view?.round ?? -1;
  const phase = view?.phase;
  useEffect(() => {
    setDraft("");
    setVotedTarget(null);
  }, [round, phase]);

  if (!view || view.game.view !== "spot_the_bot") return null;
  const g = view.game;

  const isHost = view.host_id === playerId;
  const phaseKey = `${view.round}:${view.phase}`;
  const acted = actedKey === phaseKey && (view.phase === "collecting" || view.phase === "voting");

  function sendSubmit() {
    if (!draft.trim()) return;
    submit(draft.trim());
    setActedKey(phaseKey);
  }

  function sendVote(target: string) {
    vote(target);
    setActedKey(phaseKey);
    setVotedTarget(target);
  }

  return (
    <>
      {view.phase === "lobby" && (
        <LobbyShell players={view.players} youId={playerId} isHost={isHost} onStart={start}>
          Share code <span className="font-black text-[var(--g-pop2)]">{view.room_id}</span> with
          your team. When everyone's in, the host starts the game — and one or more players are
          quietly swapped for an AI. 🤖
        </LobbyShell>
      )}

      {view.phase === "collecting" && (
        <section className="g-card space-y-4 p-7">
          <h2 className="g-display text-center text-2xl">{g.prompt}</h2>
          {acted || g.bots_writing ? (
            <div className="text-center">
              <div className="g-float text-5xl">{g.bots_writing ? "🤖" : "✅"}</div>
              <p className="mt-2 font-bold">
                {g.bots_writing
                  ? "The bots are sneaking in their answers…"
                  : "Answer locked in! Sit tight…"}
              </p>
              <p className="mt-1 font-semibold text-[var(--g-ink)]/60">
                {g.submitted_count}/{g.player_count} answered
              </p>
            </div>
          ) : (
            <>
              <textarea
                className="w-full rounded-2xl border-2 border-[var(--g-pop2)]/20 bg-white p-4 text-lg font-semibold outline-none focus:border-[var(--g-pop)]"
                rows={3}
                maxLength={280}
                placeholder="Answer like a human would…"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    sendSubmit();
                  }
                }}
              />
              <button className="g-btn w-full text-lg" onClick={sendSubmit} disabled={!draft.trim()}>
                Send it ✏️
              </button>
            </>
          )}
        </section>
      )}

      {view.phase === "voting" && (
        <section className="space-y-4">
          <div className="g-card p-5 text-center">
            <p className="font-semibold text-[var(--g-ink)]/60">The prompt was</p>
            <h2 className="g-display text-xl">{g.prompt}</h2>
            <p className="mt-2 font-extrabold text-[var(--g-pop2)]">
              {acted ? "Vote cast — who's the bot? 👀" : "Tap the answer you think the AI wrote"}
            </p>
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            {g.responses.map((r) => {
              const chosen = votedTarget === r.player_id;
              return (
                <button
                  key={r.player_id}
                  className={`g-card g-tile p-4 text-left ${acted ? "" : "g-tile-pick"} ${
                    chosen ? "g-tile-chosen" : ""
                  }`}
                  disabled={acted}
                  onClick={() => sendVote(r.player_id)}
                >
                  <p className="text-lg font-semibold">{r.text}</p>
                </button>
              );
            })}
          </div>
        </section>
      )}

      {view.phase === "reveal" && g.reveal && (
        <section className="space-y-4">
          <RevealBanner bots={g.reveal.players.filter((p) => p.is_ai)} />
          <div className="grid gap-2 sm:grid-cols-2">
            {g.reveal.players.map((p) => {
              const rivalry =
                p.is_ai && playerId
                  ? (g.rivalries[playerId] ?? []).find((r) => r.persona === p.persona)
                  : undefined;
              return (
                <div
                  key={p.id}
                  className="g-tile p-3"
                  style={p.is_ai ? { borderColor: "#7c3aed", boxShadow: "0 0 0 3px rgba(124,58,237,0.3)" } : undefined}
                >
                  {p.answer && <p className="font-semibold">{p.answer}</p>}
                  <div className="mt-1 flex items-center gap-2 text-sm">
                    <span className="font-extrabold">
                      {p.is_ai ? `🤖 ${p.persona}` : p.display_name}
                      {p.id === playerId ? " (you)" : ""}
                    </span>
                    <span
                      className="ml-auto g-chip"
                      style={{ background: "#fff0f6", color: "#ff3d81", border: "none" }}
                    >
                      {p.votes_received} 🗳️
                    </span>
                  </div>
                  {p.is_ai && p.jab && (
                    <p className="mt-2 text-sm font-bold italic text-[var(--g-ink)]/80">
                      “{p.jab}”
                    </p>
                  )}
                  {rivalry && <RivalryLine row={rivalry} />}
                </div>
              );
            })}
          </div>
          <ScoresCard rows={view.scoreboard} youId={playerId} />
          <HostContinue isHost={isHost} round={view.round} totalRounds={view.total_rounds} />
        </section>
      )}

      {view.phase === "game_over" && <GameOver scoreboard={view.scoreboard} youId={playerId} />}
    </>
  );
}

function RevealBanner({ bots }: { bots: RevealedPlayer[] }) {
  const many = bots.length > 1;
  return (
    <div className="g-card p-6 text-center">
      <p className="font-bold text-[var(--g-ink)]/60">{many ? "The bots were…" : "The bot was…"}</p>
      <div className="g-float mt-1 text-5xl">🤖</div>
      <h2 className="g-display mt-1 text-2xl">{bots.map((b) => b.persona).join(" & ")}!</h2>
    </div>
  );
}

function RivalryLine({ row }: { row: RivalryRow }) {
  return (
    <p className="mt-1 text-xs font-bold text-[var(--g-ink)]/55">
      You've caught {row.persona} {row.caught}× · they've fooled you {row.fooled}× over {row.plays}{" "}
      round{row.plays === 1 ? "" : "s"} together
    </p>
  );
}
