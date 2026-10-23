import { useEffect } from "react";
import { useNavigate, useParams } from "react-router";
import { useGamesStore } from "@/stores/games-store";
import { Countdown } from "@/components/games/countdown";
import { SpotTheBotView } from "@/components/games/spot-the-bot-view";
import { ReversePictionaryView } from "@/components/games/reverse-pictionary-view";
import { BalderdashView } from "@/components/games/balderdash-view";
import { CaptionBattleView } from "@/components/games/caption-battle-view";
import { HumanPictionaryView } from "@/components/games/human-pictionary-view";
import type { Difficulty } from "@/pages/games/types";

const DIFFICULTY_LABEL: Record<Difficulty, string> = {
  easy: "😌 Easy",
  normal: "🙂 Normal",
  hard: "😤 Hard",
  "very-hard": "🔥 Very hard",
};

export default function GamesRoom() {
  const { roomId } = useParams();
  const navigate = useNavigate();
  const view = useGamesStore((s) => s.view);
  const error = useGamesStore((s) => s.error);
  const connect = useGamesStore((s) => s.connect);
  const disconnect = useGamesStore((s) => s.disconnect);
  const dismissError = useGamesStore((s) => s.dismissError);

  useEffect(() => {
    if (roomId) connect(roomId);
    return () => disconnect();
  }, [roomId, connect, disconnect]);

  useEffect(() => {
    if (!error) return;
    const timer = setTimeout(dismissError, 3500);
    return () => clearTimeout(timer);
  }, [error, dismissError]);

  if (!view) {
    return (
      <div className="g-card p-8 text-center">
        <p className="text-lg font-bold">Joining room… 🎈</p>
        <button type="button" className="g-chip mt-4" onClick={() => navigate("/games")}>
          ← Back to games
        </button>
      </div>
    );
  }

  const showRound = view.phase !== "lobby" && view.phase !== "game_over";
  return (
    <div className="space-y-5">
      {error && (
        <div className="g-card border-l-4 border-[var(--g-pop)] p-3 text-center font-bold text-[var(--g-pop)]">
          {error}
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <button type="button" className="g-chip" onClick={() => navigate("/games")}>
          ← Leave
        </button>
        <span className="g-chip">Room {view.room_id}</span>
        <span className="g-chip">{DIFFICULTY_LABEL[view.difficulty]}</span>
        {showRound && (
          <span className="g-chip">
            Round {view.round + 1}/{view.total_rounds}
          </span>
        )}
        <span className="ml-auto">
          <Countdown endsAt={view.ends_at} />
        </span>
      </div>
      {view.game.view === "spot_the_bot" ? (
        <SpotTheBotView />
      ) : view.game.view === "balderdash" ? (
        <BalderdashView />
      ) : view.game.view === "caption_battle" ? (
        <CaptionBattleView />
      ) : view.game.view === "human_pictionary" ? (
        <HumanPictionaryView />
      ) : (
        <ReversePictionaryView />
      )}
    </div>
  );
}
