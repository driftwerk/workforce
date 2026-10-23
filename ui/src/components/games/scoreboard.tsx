import type { ScoreRow } from "@/pages/games/types";

export function Scoreboard({ rows, youId }: { rows: ScoreRow[]; youId: string | null }) {
  return (
    <div className="space-y-2">
      {rows.map((row, i) => (
        <div
          key={row.player_id}
          className="g-tile flex items-center gap-3 px-4 py-3"
          style={{ animationDelay: `${i * 60}ms` }}
        >
          <span className="w-8 text-center text-xl font-black">
            {i === 0 ? "👑" : `#${i + 1}`}
          </span>
          <span className="g-avatar h-9 w-9 text-xl">{row.avatar}</span>
          <span className="font-extrabold">
            {row.display_name}
            {row.player_id === youId ? " (you)" : ""}
          </span>
          {row.is_ai && (
            <span className="g-chip" style={{ color: "#6d28d9", background: "#ede9fe", border: "none" }}>
              🤖 AI
            </span>
          )}
          <span className="ml-auto text-2xl font-black tabular-nums">{row.score}</span>
        </div>
      ))}
    </div>
  );
}
