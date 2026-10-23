import { useGamesStore } from "@/stores/games-store";

// Host-only button that skips the reveal timer to the next round (or game-end).
export function HostContinue({
  isHost,
  round,
  totalRounds,
}: {
  isHost: boolean;
  round: number;
  totalRounds: number;
}) {
  const skipReveal = useGamesStore((s) => s.skipReveal);
  if (!isHost) return null;
  const last = round + 1 >= totalRounds;
  return (
    <button type="button" className="g-btn w-full text-lg" onClick={skipReveal}>
      {last ? "Finish ▶" : "Next round ▶"}
    </button>
  );
}
