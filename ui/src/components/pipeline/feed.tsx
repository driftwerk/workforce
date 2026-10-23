import { cn } from "@/lib/utils";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { type FeedEvent, describe } from "@/lib/pipeline/lineage";
import { TONE_FILL } from "./tone";

function clock(at: string): string {
  const d = new Date(at);
  if (Number.isNaN(d.getTime())) return at;
  return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function day(at: string): string {
  const d = new Date(at);
  return Number.isNaN(d.getTime()) ? "" : d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function Feed({ events, at }: { events: FeedEvent[]; at: Date | null }) {
  const shown = (at ? events.filter((e) => new Date(e.at) <= at) : events).slice(0, 80);
  return (
    <Card className="flex max-h-[680px] flex-col">
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">Transitions</CardTitle>
      </CardHeader>
      <CardContent className="min-h-0 flex-1 overflow-y-auto">
        {shown.length === 0 ? (
          <p className="py-6 text-center text-xs text-muted-foreground">Nothing has moved yet.</p>
        ) : (
          <ol className="space-y-2 text-xs">
            {shown.map((e, i) => {
              const tone = e.position?.tone ?? e.annotation?.tone ?? "neutral";
              const newDay = i === 0 || day(e.at) !== day(shown[i - 1]?.at ?? "");
              return (
                <li key={`${e.item}-${e.at}-${e.event}`}>
                  {newDay && (
                    <div className="mb-1 mt-2 text-[10px] font-semibold uppercase tracking-wide text-muted-foreground/70">
                      {day(e.at)}
                    </div>
                  )}
                  <div className="flex items-start gap-2">
                    <span className={cn("mt-1.5 h-2 w-2 shrink-0 rounded-full", TONE_FILL[tone])} aria-hidden />
                    <div className="min-w-0 flex-1">
                      <div className="flex items-baseline justify-between gap-2">
                        <span className="font-mono font-semibold">{e.item}</span>
                        <span className="shrink-0 font-mono text-[10px] tabular-nums text-muted-foreground">{clock(e.at)}</span>
                      </div>
                      <div className="text-muted-foreground">
                        <span className="text-foreground/80">
                          {e.stage}/{e.event}
                        </span>
                        {e.position && <span> → {e.position.label}</span>}
                        {e.annotation && e.annotation.label !== `${e.stage}/${e.event}` && <span> · {e.annotation.label}</span>}
                        {!e.position && !e.annotation && <span className="text-muted-foreground/60"> (no rule)</span>}
                        {describe(e) && <span className="block truncate">{describe(e)}</span>}
                      </div>
                    </div>
                  </div>
                </li>
              );
            })}
          </ol>
        )}
      </CardContent>
    </Card>
  );
}
