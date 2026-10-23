import { useEffect, useState } from "react";
import { useGamesStore } from "@/stores/games-store";
import { LobbyShell } from "@/components/games/lobby-shell";
import { ScoresCard } from "@/components/games/scores-card";
import { GameOver } from "@/components/games/game-over";
import { HostContinue } from "@/components/games/host-continue";

export function CaptionBattleView() {
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

  if (!view || view.game.view !== "caption_battle") return null;
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
          Each round you'll get a silly prompt and write the funniest caption you can — the AI plays
          too. Then everyone votes for the best one. Share code{" "}
          <span className="font-black text-[var(--g-pop2)]">{view.room_id}</span> with your team! 📝
        </LobbyShell>
      )}

      {view.phase === "collecting" && (
        <section className="g-card space-y-4 p-7">
          <p className="text-center font-semibold text-[var(--g-ink)]/60">Write the funniest…</p>
          <h2 className="g-display text-center text-2xl">{g.prompt}</h2>
          {acted ? (
            <div className="text-center">
              <div className="g-float text-5xl">{g.bots_writing ? "🤖" : "✅"}</div>
              <p className="mt-2 font-bold">
                {g.bots_writing ? "A bot is crafting a caption…" : "Caption locked in! Sit tight…"}
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
                placeholder="Your funniest answer…"
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
                Send it 😄
              </button>
            </>
          )}
        </section>
      )}

      {view.phase === "voting" && (
        <section className="space-y-4">
          <div className="g-card p-5 text-center">
            <h2 className="g-display text-xl">{g.prompt}</h2>
            <p className="mt-2 font-extrabold text-[var(--g-pop2)]">
              {acted ? "Vote cast — who's the funniest? 🏆" : "Tap the caption that made you laugh most"}
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
          <div className="space-y-2">
            {g.reveal.entries.map((entry, i) => (
              <div
                key={i}
                className="g-tile p-3"
                style={
                  entry.is_winner
                    ? { borderColor: "#f59e0b", boxShadow: "0 0 0 3px rgba(245,158,11,0.35)" }
                    : undefined
                }
              >
                <p className="font-semibold">{entry.text}</p>
                <div className="mt-1 flex items-center gap-2 text-sm">
                  <span className="font-extrabold">
                    {entry.is_winner ? "🏆 " : ""}
                    {entry.is_ai
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
              </div>
            ))}
          </div>
          <ScoresCard rows={view.scoreboard} youId={playerId} />
          <HostContinue isHost={isHost} round={view.round} totalRounds={view.total_rounds} />
        </section>
      )}

      {view.phase === "game_over" && <GameOver scoreboard={view.scoreboard} youId={playerId} />}
    </>
  );
}
