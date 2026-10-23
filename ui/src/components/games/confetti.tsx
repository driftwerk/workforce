import { useMemo } from "react";

const COLORS = ["#ff3d81", "#7c3aed", "#22d3a6", "#ffd166", "#18a0fb", "#fca5f1"];

export function Confetti({ pieces = 90 }: { pieces?: number }) {
  const bits = useMemo(
    () =>
      Array.from({ length: pieces }, (_, i) => ({
        left: Math.random() * 100,
        delay: Math.random() * 0.8,
        duration: 2.4 + Math.random() * 2.4,
        color: COLORS[i % COLORS.length],
      })),
    [pieces],
  );
  return (
    <div className="g-confetti" aria-hidden>
      {bits.map((bit, i) => (
        <i
          key={i}
          style={{
            left: `${bit.left}vw`,
            background: bit.color,
            animationDelay: `${bit.delay}s`,
            animationDuration: `${bit.duration}s`,
          }}
        />
      ))}
    </div>
  );
}
