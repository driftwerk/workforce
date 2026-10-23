import type { PlayerView } from "@/pages/games/types";

export function PlayerGrid({ players, youId }: { players: PlayerView[]; youId: string | null }) {
  return (
    <div className="flex flex-wrap justify-center gap-3">
      {players.map((p) => (
        <div
          key={p.id}
          className={`g-tile flex items-center gap-2 px-3 py-2 ${p.connected ? "" : "opacity-50"}`}
        >
          <span className="g-avatar h-8 w-8 text-lg">{p.avatar || "🙂"}</span>
          <span className="font-extrabold">{p.id === youId ? "You" : p.label || "Player"}</span>
        </div>
      ))}
    </div>
  );
}
