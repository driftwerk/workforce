import { useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { cardsPerColumn, windowReach } from "@/lib/pipeline/columns";
import { type Column, type PipelineDefinition, type Rule, type Tone, clockEnds, milestoneLanes, places } from "@/lib/pipeline/definition";
import { whenText } from "@/lib/pipeline/diagram";

const TONE_BADGE: Record<Tone, "default" | "success" | "warning" | "info" | "destructive" | "secondary"> = {
  active: "default",
  good: "success",
  warn: "warning",
  healing: "info",
  bad: "destructive",
  neutral: "secondary",
};

function ruleText(rule: Rule): string {
  const when = whenText(rule);
  return when ? `when ${when}` : "otherwise";
}

function ColumnName({ column }: { column: Column }) {
  return (
    <>
      {column.label} <span className="font-mono text-muted-foreground">({column.id})</span>
      {column.window && <span className="text-muted-foreground"> · shows {windowReach(column.window)}</span>}
    </>
  );
}

/**
 * The definition in plain language: lanes, exits and, per event, where it
 * sends an item. Read-only here — to change it, ask an agent (it has
 * pipeline.define) or write the document through the store.
 */
export function DefinitionPanel({ def, scope, id, version }: { def: PipelineDefinition; scope: string; id: string; version: number }) {
  const [open, setOpen] = useState(false);
  const label = (column: string) =>
    def.lanes.find((l) => l.id === column)?.label ?? def.exits?.find((e) => e.id === column)?.label ?? column;
  const isLane = (column: string) => def.lanes.some((l) => l.id === column);
  const origin = def.timings?.origin;
  const milestones = milestoneLanes(def);
  const ends = clockEnds(def);

  return (
    <Card>
      <CardHeader className="pb-2">
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex w-full items-center justify-between text-left"
          aria-expanded={open}
        >
          <CardTitle className="text-sm">Definition</CardTitle>
          <span className="flex items-center gap-2 text-xs text-muted-foreground">
            <span className="font-mono">
              {scope}/{id} · v{version}
            </span>
            {open ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
          </span>
        </button>
      </CardHeader>
      {open && (
        <CardContent className="space-y-4 text-xs">
          {def.description && <p className="text-muted-foreground">{def.description}</p>}
          <div className="grid gap-4 sm:grid-cols-2">
            <div>
              <h4 className="mb-1 font-semibold uppercase tracking-wide text-muted-foreground">Lanes</h4>
              <ol className="space-y-0.5">
                {def.lanes.map((l, i) => (
                  <li key={l.id}>
                    <span className="mr-1.5 font-mono text-muted-foreground/60">{i + 1}</span>
                    <ColumnName column={l} />
                  </li>
                ))}
              </ol>
              <p className="mt-2 text-muted-foreground">Columns show {cardsPerColumn(def)} cards before they scroll.</p>
            </div>
            <div>
              <h4 className="mb-1 font-semibold uppercase tracking-wide text-muted-foreground">Exits</h4>
              {def.exits && def.exits.length > 0 ? (
                <ul className="space-y-0.5">
                  {def.exits.map((e) => (
                    <li key={e.id}>
                      <ColumnName column={e} />
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-muted-foreground">None — every item stays in the flow.</p>
              )}
              {def.rungs && def.rungs.length > 0 && (
                <p className="mt-2 text-muted-foreground">
                  Ladder: <span className="font-mono">{def.rungs.join(" · ")}</span>
                </p>
              )}
              <p className="mt-2 text-muted-foreground">
                Timed from{" "}
                {origin ? (
                  <>
                    {origin.label} (<span className="font-mono">{origin.field}</span>)
                  </>
                ) : (
                  "when first recorded"
                )}
                {milestones.length > 0 && <> · milestones {milestones.map((l) => l.label).join(", ")}</>} ·{" "}
                {ends.length > 0 ? `clock stops at ${ends.map(label).join(", ")}` : "the clock never stops"}
              </p>
            </div>
          </div>
          <div>
            <h4 className="mb-1 font-semibold uppercase tracking-wide text-muted-foreground">Events</h4>
            <table className="w-full">
              <tbody>
                {Object.entries(def.events).map(([key, rules]) => (
                  <tr key={key} className="border-t align-top">
                    <td className="py-1 pr-3 font-mono">{key}</td>
                    <td className="py-1">
                      <ul className="space-y-0.5">
                        {rules.map((r, i) => (
                          <li key={i} className="flex flex-wrap items-center gap-1.5">
                            <span className={cn("text-muted-foreground", rules.length === 1 && "hidden")}>{ruleText(r)} →</span>
                            <Badge variant={TONE_BADGE[r.tone ?? "neutral"]} className="px-1.5 py-0 text-[10px]">
                              {r.label ?? (places(r) ? label(r.column) : key)}
                            </Badge>
                            <span className="text-muted-foreground/70">
                              {places(r) ? (isLane(r.column) ? "lane" : "exit") : "annotates the card"}
                            </span>
                            {r.label_field && (
                              <span className="text-muted-foreground/70">
                                · label from <span className="font-mono">{r.label_field}</span>
                              </span>
                            )}
                            {r.rung_field && (
                              <span className="text-muted-foreground/70">
                                · rung from <span className="font-mono">{r.rung_field}</span>
                                {r.rung_passed ? " (passed)" : ""}
                              </span>
                            )}
                            {r.claim && (
                              <span className="text-muted-foreground/70">
                                · claims the item
                                {r.lease_field && (
                                  <>
                                    {" "}
                                    (lease from <span className="font-mono">{r.lease_field}</span>)
                                  </>
                                )}
                              </span>
                            )}
                            {r.due_field && (
                              <span className="text-muted-foreground/70">
                                · due from <span className="font-mono">{r.due_field}</span>
                              </span>
                            )}
                          </li>
                        ))}
                      </ul>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {def.sources && def.sources.length > 0 && (
            <p className="text-muted-foreground">
              Fed by: <span className="font-mono">{def.sources.join(", ")}</span>
            </p>
          )}
          <p className="text-muted-foreground">
            To change this pipeline, ask an agent — "rename the {def.lanes[0]?.label} lane", "add an exit for
            rejected" — it redefines it with <span className="font-mono">pipeline.define</span>, validated.
          </p>
        </CardContent>
      )}
    </Card>
  );
}
