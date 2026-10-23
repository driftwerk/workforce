import { useEffect, useState } from "react";

export function Countdown({ endsAt }: { endsAt: number | null }) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now() / 1000), 500);
    return () => clearInterval(timer);
  }, []);
  if (endsAt == null) return null;
  const remaining = Math.max(0, Math.ceil(endsAt - now));
  return (
    <span className="g-chip g-pulse" aria-label={`${remaining} seconds left`}>
      ⏳ {remaining}s
    </span>
  );
}
