import type { ReactNode } from "react";
import { PlayerGrid } from "@/components/games/player-grid";
import type { PlayerView } from "@/pages/games/types";

export function LobbyShell({
  players,
  youId,
  isHost,
  onStart,
  children,
}: {
  players: PlayerView[];
  youId: string | null;
  isHost: boolean;
  onStart: () => void;
  children: ReactNode;
}) {
  return (
    <section className="g-card space-y-5 p-7 text-center">
      <h2 className="g-display text-2xl">Waiting room</h2>
      <p className="font-semibold text-[var(--g-ink)]/70">{children}</p>
      <PlayerGrid players={players} youId={youId} />
      {isHost ? (
        <button className="g-btn text-lg" onClick={onStart}>
          🚀 Start the game
        </button>
      ) : (
        <p className="g-chip mx-auto w-fit g-pulse">Waiting for the host to start…</p>
      )}
    </section>
  );
}
