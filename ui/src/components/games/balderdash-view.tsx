import { useEffect, useState } from "react";
import { useGamesStore } from "@/stores/games-store";
import { LobbyShell } from "@/components/games/lobby-shell";
import { ScoresCard } from "@/components/games/scores-card";
import { GameOver } from "@/components/games/game-over";
import { HostContinue } from "@/components/games/host-continue";
import type { RivalryRow } from "@/pages/games/types";

export function BalderdashView() {
  const view = useGamesStore((s) => s.view);
  const playerId = useGamesStore((s) => s.playerId);
  const start = useGamesStore((s) => s.start);
  const submit = useGamesStore((s) => s.submit);
  const vote = useGamesStore((s) => s.vote);

  const [draft, setDraft] = useState("");
  const [actedKey, setActedKey] = useState("");
  const [votedId, setVotedId] = useState<string | null>(null);

  const round = view?.round ?? -1;
  const phase = view?.phase;
  useEffect(() => {
    setDraft("");
    setVotedId(null);
  }, [round, phase]);

  if (!view || view.game.view !== "balderdash") return null;
  const g = view.game;

  const isHost = view.host_id === playerId;
  const phaseKey = `${view.round}:${view.phase}`;
  const acted = actedKey === phaseKey && (view.phase === "collecting" || view.phase === "voting");

  function sendSubmit() {
    if (!draft.trim()) return;
    submit(draft.trim());
    setActedKey(phaseKey);
  }

  function sendVote(id: string) {
    vote(id);
    setActedKey(phaseKey);
    setVotedId(id);
  }

  return (
    <>
      {view.phase === "lobby" && (
        <LobbyShell players={view.players} youId={playerId} isHost={isHost} onStart={start}>
          Each round you'll get an obscure subject and write a <em>fake</em> {g.noun} — the AI bluffs
          too. Then everyone votes for the one they think is real. Share code{" "}
          <span className="font-black text-[var(--g-pop2)]">{view.room_id}</span> with your team! 📖
        </LobbyShell>
      )}

      {view.phase === "collecting" && (
        <section className="g-card space-y-4 p-7">
          <p className="text-center font-semibold text-[var(--g-ink)]/60">Bluff a {g.noun} for</p>
          <h2 className="g-display text-center text-3xl">{g.word}</h2>
          {acted ? (
            <div className="text-center">
              <div className="g-float text-5xl">{g.bots_writing ? "🤖" : "✅"}</div>
              <p className="mt-2 font-bold">
                {g.bots_writing
                  ? `A bot is concocting a ${g.noun}…`
                  : "Bluff locked in! Sit tight…"}
              </p>
              <p className="mt-1 font-semibold text-[var(--g-ink)]/60">
                {g.submitted_count}/{g.player_count} in
              </p>
            </div>
          ) : (
            <>
              <textarea
                className="w-full rounded-2xl border-2 border-[var(--g-pop2)]/20 bg-white p-4 text-lg font-semibold outline-none focus:border-[var(--g-pop)]"
                rows={2}
                maxLength={200}
                placeholder={`Write a convincing fake ${g.noun}…`}
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
                Submit bluff ✍️
              </button>
            </>
          )}
        </section>
      )}

      {view.phase === "voting" && (
        <section className="space-y-4">
          <div className="g-card p-5 text-center">
            <p className="font-semibold text-[var(--g-ink)]/60">The word was</p>
            <h2 className="g-display text-2xl">{g.word}</h2>
            <p className="mt-2 font-extrabold text-[var(--g-pop2)]">
              {acted ? "Vote cast — which is real? 👀" : `Tap the ${g.noun} you think is the real one`}
            </p>
          </div>
          <div className="space-y-3">
            {g.options.map((o) => {
              const chosen = votedId === o.id;
              return (
                <button
                  key={o.id}
                  className={`g-card g-tile w-full p-4 text-left ${acted ? "" : "g-tile-pick"} ${
                    chosen ? "g-tile-chosen" : ""
                  }`}
                  disabled={acted}
                  onClick={() => sendVote(o.id)}
                >
                  <p className="text-lg font-semibold">{o.text}</p>
                </button>
              );
            })}
          </div>
        </section>
      )}

      {view.phase === "reveal" && g.reveal && (
        <section className="space-y-4">
          <div className="g-card p-6 text-center">
            <p className="font-bold text-[var(--g-ink)]/60">The real {g.noun} of {g.word} was</p>
            <h2 className="g-display mt-1 text-2xl">{g.reveal.real_text}</h2>
          </div>
          <div className="space-y-2">
            {g.reveal.entries.map((entry, i) => {
              const rivalry =
                entry.is_ai && playerId
                  ? (g.rivalries[playerId] ?? []).find((r) => r.persona === entry.persona)
                  : undefined;
              return (
                <div
                  key={i}
                  className="g-tile p-3"
                  style={
                    entry.is_real
                      ? { borderColor: "#16a34a", boxShadow: "0 0 0 3px rgba(22,163,74,0.3)" }
                      : undefined
                  }
                >
                  <p className="font-semibold">{entry.text}</p>
                  <div className="mt-1 flex items-center gap-2 text-sm">
                    <span className="font-extrabold">
                      {entry.is_real
                        ? `📖 The real ${g.noun}`
                        : entry.is_ai
                          ? `🤖 ${entry.persona}`
                          : (entry.author_display_name ?? "Someone")}
                    </span>
                    <span
                      className="ml-auto g-chip"
                      style={{ background: "#fff0f6", color: "#ff3d81", border: "none" }}
                    >
                      {entry.votes_received} 🗳️
                    </span>
                  </div>
                  {entry.is_ai && entry.jab && (
                    <p className="mt-2 text-sm font-bold italic text-[var(--g-ink)]/80">
                      “{entry.jab}”
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

function RivalryLine({ row }: { row: RivalryRow }) {
  return (
    <p className="mt-1 text-xs font-bold text-[var(--g-ink)]/55">
      You've seen through {row.persona} {row.caught}× · they've fooled you {row.fooled}× over{" "}
      {row.plays} round{row.plays === 1 ? "" : "s"} together
    </p>
  );
}
