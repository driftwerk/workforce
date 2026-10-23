import { useEffect, useRef } from "react";
import { Pause, Play, Radio, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

export interface ReplayState {
  /** Replaying (a clock other than now) or live. */
  active: boolean;
  playing: boolean;
  /** Replay clock, ms since epoch. */
  t: number;
  from: number;
  to: number;
  speed: number;
}

export const SPEEDS = [10, 60, 300, 1800] as const;

interface Props {
  state: ReplayState;
  onChange: (next: ReplayState) => void;
  /** The replay's full history is still loading. */
  loading?: boolean;
}

function stamp(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

/**
 * Live, or a replay of the window the records cover at up to 1800×. The
 * clock advances `speed` seconds per real second while playing, and the
 * board and feed take their time from it.
 */
export function ReplayBar({ state, onChange, loading }: Props) {
  const stateRef = useRef(state);
  stateRef.current = state;

  useEffect(() => {
    if (!state.active || !state.playing) return;
    let last = performance.now();
    const id = window.setInterval(() => {
      const now = performance.now();
      const elapsed = (now - last) / 1000;
      last = now;
      const s = stateRef.current;
      const next = Math.min(s.to, s.t + elapsed * s.speed * 1000);
      onChange({ ...s, t: next, playing: next < s.to });
    }, 100);
    return () => window.clearInterval(id);
  }, [state.active, state.playing, onChange]);

  const span = Math.max(1, state.to - state.from);
  const progress = ((state.t - state.from) / span) * 100;

  return (
    <div className="flex flex-wrap items-center gap-3 rounded-lg border bg-card px-3 py-2 text-xs">
      <Button
        variant={state.active ? "outline" : "default"}
        size="sm"
        className="h-7 gap-1.5"
        onClick={() => onChange({ ...state, active: false, playing: false, t: state.to })}
      >
        <Radio className={cn("h-3.5 w-3.5", !state.active && "motion-safe:animate-pulse")} />
        Live
      </Button>
      <div className="flex items-center gap-1">
        <Button
          variant="ghost"
          size="sm"
          className="h-7 w-7 p-0"
          aria-label={state.playing ? "Pause replay" : "Play replay"}
          onClick={() =>
            onChange({
              ...state,
              active: true,
              playing: !state.playing,
              t: !state.playing && state.t >= state.to ? state.from : state.t,
            })
          }
        >
          {state.playing ? <Pause className="h-3.5 w-3.5" /> : <Play className="h-3.5 w-3.5" />}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 w-7 p-0"
          aria-label="Restart replay"
          onClick={() => onChange({ ...state, active: true, playing: true, t: state.from })}
        >
          <RotateCcw className="h-3.5 w-3.5" />
        </Button>
      </div>
      <label className="flex min-w-[260px] flex-1 items-center gap-2">
        <span className="sr-only">Replay position</span>
        <input
          id="pipeline-replay-position"
          type="range"
          min={state.from}
          max={state.to}
          step={1000}
          value={state.t}
          onChange={(e) => onChange({ ...state, active: true, playing: false, t: Number(e.target.value) })}
          className="h-1.5 w-full cursor-pointer accent-primary"
          style={{ background: `linear-gradient(to right, var(--color-primary) ${progress}%, var(--color-muted) ${progress}%)` }}
        />
      </label>
      <span className="font-mono tabular-nums text-muted-foreground">
        {state.active ? (loading ? "loading history…" : stamp(state.t)) : "now"}
      </span>
      <div className="flex items-center gap-1" role="group" aria-label="replay speed">
        {SPEEDS.map((s) => (
          <button
            key={s}
            type="button"
            onClick={() => onChange({ ...state, speed: s })}
            className={cn(
              "rounded px-1.5 py-0.5 font-mono tabular-nums",
              state.speed === s ? "bg-primary/15 text-primary" : "text-muted-foreground hover:text-foreground",
            )}
          >
            {s}×
          </button>
        ))}
      </div>
    </div>
  );
}
