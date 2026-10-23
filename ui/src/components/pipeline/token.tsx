import { ExternalLink } from "lucide-react";
import { cn, formatRelativeTime } from "@/lib/utils";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { type Activity, activityCue, activityDetail } from "@/lib/pipeline/activity";
import type { PipelineDefinition, Position } from "@/lib/pipeline/definition";
import { type LineageBody, type StandingAnnotation, itemLink, passedRungs, summary, lastRun } from "@/lib/pipeline/lineage";
import { AnnotationChip } from "./annotation-chip";
import { cardProps, revealCard } from "./card-focus";
import { TONE_FILL, TONE_NAME } from "./tone";

const TONE_RING: Record<Position["tone"], string> = {
  active: "ring-primary/40",
  good: "ring-success/40",
  warn: "ring-warning/40",
  healing: "ring-sky-500/40",
  bad: "ring-red-500/40",
  neutral: "ring-transparent",
};

export interface TokenProps {
  def: PipelineDefinition;
  id: string;
  /** The record's document id, handed to `onOpen`. */
  docId: string;
  onOpen: (docId: string) => void;
  body: LineageBody;
  position: Position;
  enteredAt?: string;
  activity: Activity;
  /** The annotation showing on the item since it last moved. */
  annotation: StandingAnnotation | null;
  /** Just moved into this column: play the arrival animation. */
  arrived?: boolean;
  /** The replay clock, when replaying; else null. */
  at: Date | null;
  /** The live board's clock, refreshed while not replaying. */
  now: Date;
}

export function Token({ def, id, docId, onOpen, body, position, enteredAt, activity, annotation, arrived, at, now }: TokenProps) {
  const clock = at ?? now;
  const cue = activityCue(activity, clock);
  const link = itemLink(def, body);
  const text = summary(def, body);
  const run = lastRun(body);
  const ladder = def.rungs && def.rungs.length > 0 && (position.rung || passedRungs(def, body, at).size > 0);
  const passed = ladder ? passedRungs(def, body, at) : null;
  const halted = position.tone === "bad";
  const columnLabel = def.lanes.find((l) => l.id === position.column)?.label ?? def.exits?.find((e) => e.id === position.column)?.label;
  const detail = activityDetail(activity);
  const open = (card: HTMLElement) => {
    revealCard(card);
    onOpen(docId);
  };

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <div
          {...cardProps(docId)}
          role="button"
          tabIndex={0}
          aria-label={[`Open ${id}`, cue, annotation && `${TONE_NAME[annotation.tone]}: ${annotation.label}`].filter(Boolean).join(", ")}
          onClick={(e) => open(e.currentTarget)}
          onKeyDown={(e) => {
            if (e.key === "Enter") open(e.currentTarget);
            if (e.key === " ") {
              e.preventDefault();
              open(e.currentTarget);
            }
          }}
          className={cn(
            "relative flex w-full cursor-pointer flex-col gap-1 overflow-hidden rounded-md border bg-card px-2.5 py-2 text-left text-xs shadow-sm ring-2 transition-shadow focus-visible:outline-none focus-visible:ring-primary",
            TONE_RING[position.tone],
            arrived && "motion-safe:animate-[wf-arrive_900ms_ease-out]",
            halted && "motion-safe:animate-pulse",
          )}
        >
          <span className={cn("absolute inset-y-0 left-0 w-1", TONE_FILL[position.tone])} aria-hidden />
          <div className="flex items-center justify-between gap-2 pl-1.5">
            <span className="min-w-0 truncate font-mono font-semibold tabular-nums">{id}</span>
            {link && (
              <a
                href={link.url}
                target="_blank"
                rel="noreferrer"
                className="inline-flex shrink-0 items-center gap-0.5 font-mono text-muted-foreground hover:text-foreground"
                onClick={(e) => e.stopPropagation()}
                onKeyDown={(e) => e.stopPropagation()}
              >
                {link.label}
                <ExternalLink className="h-3 w-3" />
              </a>
            )}
          </div>
          {text && <p className="line-clamp-2 pl-1.5 leading-snug text-muted-foreground">{text}</p>}
          <div className="flex items-center justify-between gap-2 pl-1.5 text-[10px] text-muted-foreground">
            <span className="min-w-0 truncate">{position.label !== columnLabel ? position.label : ""}</span>
            <span className="inline-flex shrink-0 items-center gap-1 whitespace-nowrap tabular-nums">
              {activity.kind === "working" && activity.until && (
                <span aria-hidden className="h-1.5 w-1.5 rounded-full bg-primary motion-safe:animate-pulse" />
              )}
              {cue ?? (enteredAt ? formatRelativeTime(enteredAt, clock) : "")}
            </span>
          </div>
          {annotation && <AnnotationChip annotation={annotation} clock={clock} />}
          {passed && def.rungs && (
            <ol className="flex items-center gap-1 pl-1.5" aria-label="rungs">
              {def.rungs.map((rung) => {
                const done = passed.has(rung);
                const current = position.rung === rung && !done;
                return (
                  <li
                    key={rung}
                    className={cn(
                      "rounded-sm border px-1 font-mono text-[10px] tabular-nums",
                      done && "border-success/50 bg-success/20 text-success",
                      current && "border-primary/50 bg-primary/10 text-primary",
                      !done && !current && "border-border text-muted-foreground/60",
                    )}
                  >
                    {rung}
                  </li>
                );
              })}
            </ol>
          )}
        </div>
      </TooltipTrigger>
      <TooltipContent side="right" className="max-w-xs text-xs">
        <div className="space-y-1">
          <div className="font-semibold">
            {id} — {position.label}
          </div>
          {text && <div>{text}</div>}
          {annotation && <div>{annotation.label}</div>}
          {detail && <div className="text-muted-foreground">{detail}</div>}
          {run && <div className="font-mono text-muted-foreground">run {run.slice(0, 8)}</div>}
          {body.updated_at && <div className="text-muted-foreground">updated {formatRelativeTime(body.updated_at)}</div>}
        </div>
      </TooltipContent>
    </Tooltip>
  );
}
