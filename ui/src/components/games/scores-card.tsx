import { Scoreboard } from "@/components/games/scoreboard";
import type { ScoreRow } from "@/pages/games/types";

export function ScoresCard({ rows, youId }: { rows: ScoreRow[]; youId: string | null }) {
  return (
    <div className="g-card p-5">
      <h3 className="g-display mb-3 text-lg">Scores</h3>
      <Scoreboard rows={rows} youId={youId} />
    </div>
  );
}
