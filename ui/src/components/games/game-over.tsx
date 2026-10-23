import { Link } from "react-router";
import { Scoreboard } from "@/components/games/scoreboard";
import { Confetti } from "@/components/games/confetti";
import type { ScoreRow } from "@/pages/games/types";

export function GameOver({ scoreboard, youId }: { scoreboard: ScoreRow[]; youId: string | null }) {
  const winner = scoreboard[0];
  return (
    <section className="space-y-5">
      <Confetti />
      <div className="g-card p-7 text-center">
        <div className="g-float text-6xl">🏆</div>
        <h2 className="g-display mt-2 text-3xl">
          {winner ? `${winner.avatar} ${winner.display_name} wins!` : "Game over!"}
        </h2>
      </div>
      <div className="g-card p-5">
        <Scoreboard rows={scoreboard} youId={youId} />
      </div>
      <Link to="/games" className="g-btn g-btn-mint w-full text-lg">
        🎈 Back to the lobby
      </Link>
    </section>
  );
}
