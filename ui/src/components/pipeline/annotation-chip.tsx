import { cn, formatRelativeTime } from "@/lib/utils";
import type { StandingAnnotation } from "@/lib/pipeline/lineage";
import { TONE_FILL } from "./tone";

/** An item's standing annotation on its card: tone, label and how long ago it was noted at `clock`. */
export function AnnotationChip({ annotation, clock }: { annotation: StandingAnnotation; clock: Date }) {
  return (
    <div className="flex min-w-0 items-center gap-1.5 pl-1.5 text-[10px] text-muted-foreground">
      <span aria-hidden className={cn("h-1.5 w-1.5 shrink-0 rounded-full", TONE_FILL[annotation.tone])} />
      <span className="min-w-0 flex-1 truncate text-foreground/80">{annotation.label}</span>
      <span className="shrink-0 tabular-nums">{formatRelativeTime(annotation.at, clock)}</span>
    </div>
  );
}
