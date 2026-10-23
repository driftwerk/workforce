import { ChevronRight } from "lucide-react";
import { cn, formatDateTime } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Skeleton } from "@/components/ui/skeleton";
import { useNow } from "@/hooks/use-now";
import { usePipelineItemTimings } from "@/hooks/use-pipeline";
import type { ColumnTimes, Visit } from "@/lib/api/pipelines";
import { count } from "@/lib/pipeline/costs";
import {
  durationText,
  live,
  milestoneReadings,
  originText,
  timingNotes,
  timingsErrorText,
  visitLabel,
  workShare,
} from "@/lib/pipeline/timings";
import type { Item } from "./board";
import { CoverageNotes } from "./coverage-notes";
import { Stat } from "./stat";

export function ItemTimingsPanel({
  scope,
  id,
  item,
  itemLabel,
}: {
  scope: string;
  id: string;
  item: Item;
  itemLabel: string;
}) {
  const { data, error } = usePipelineItemTimings(scope, id, item.id, item.body.updated_at);
  const fetched = data?.item === item.id ? data : undefined;
  const now = useNow(fetched?.total.open ?? false);

  if (!fetched) {
    return error ? (
      <p className="text-sm text-muted-foreground">{timingsErrorText(error)}</p>
    ) : (
      <Skeleton className="h-40 w-full" />
    );
  }

  const timings = live(fetched, now);
  const { total, coverage } = timings;
  const workKnown = !coverage.work_unavailable;
  const milestones = milestoneReadings(timings);
  return (
    <section aria-label="time" className="space-y-4">
      <div className="flex items-center gap-2">
        <h3 className="text-sm font-semibold">Time</h3>
        {total.open && <Badge variant="info">so far</Badge>}
      </div>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Stat
          label="Total"
          value={durationText(total.elapsed_ms)}
          hint={`since ${originText(timings).toLowerCase()} ${formatDateTime(timings.origin.at)}`}
        />
        {timings.origin.recorded && (
          <Stat
            label="In the pipeline"
            value={durationText(total.in_pipeline_ms)}
            hint={`since ${formatDateTime(timings.started_at)}`}
          />
        )}
        {workKnown && <Stat label="Doing work" value={durationText(total.work_ms)} />}
        {workKnown && <Stat label="Waiting" value={durationText(total.waiting_ms)} tone="muted" />}
      </div>
      {milestones.length > 0 && (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          {milestones.map((milestone) => (
            <Stat key={milestone.label} label={milestone.label} value={milestone.value} hint={milestone.hint} />
          ))}
        </div>
      )}
      <CoverageNotes notes={timingNotes(timings, itemLabel)} />
      {timings.columns.length > 0 && <Stages columns={timings.columns} workKnown={workKnown} />}
      {timings.visits.length > 0 && <Timeline visits={timings.visits} workKnown={workKnown} />}
    </section>
  );
}

function ShareBar({ share }: { share: number | null }) {
  return (
    <div aria-hidden className="flex h-1.5 w-24 overflow-hidden rounded-full bg-muted">
      {share !== null && (
        <>
          <div className="bg-primary" style={{ width: `${share * 100}%` }} />
          <div className="flex-1 bg-muted-foreground/30" />
        </>
      )}
    </div>
  );
}

function Stages({ columns, workKnown }: { columns: ColumnTimes[]; workKnown: boolean }) {
  return (
    <div className="space-y-2">
      {workKnown && (
        <p className="flex items-center gap-3 text-xs text-muted-foreground">
          <span className="inline-flex items-center gap-1">
            <span aria-hidden className="h-2 w-2 rounded-full bg-primary" />
            work
          </span>
          <span className="inline-flex items-center gap-1">
            <span aria-hidden className="h-2 w-2 rounded-full bg-muted-foreground/30" />
            waiting
          </span>
        </p>
      )}
      <div className="overflow-x-auto">
        <table className="w-full text-xs">
          <thead>
            <tr className="border-b text-left text-muted-foreground">
              <th className="py-1.5 pr-3 font-medium">Stage</th>
              <th className="py-1.5 pr-3 text-right font-medium">Visits</th>
              <th className="py-1.5 pr-3 text-right font-medium">Time</th>
              {workKnown && (
                <>
                  <th className="py-1.5 pr-3 text-right font-medium">Work</th>
                  <th className="py-1.5 pr-3 text-right font-medium">Waiting</th>
                  <th className="py-1.5">
                    <span className="sr-only">Share of time working</span>
                  </th>
                </>
              )}
            </tr>
          </thead>
          <tbody>
            {columns.map((column) => (
              <tr key={column.column} className="border-b last:border-0">
                <td className="py-1.5 pr-3 font-medium">{column.label}</td>
                <td className="py-1.5 pr-3 text-right tabular-nums">{column.visits}</td>
                <td className="py-1.5 pr-3 text-right tabular-nums">{durationText(column.elapsed_ms)}</td>
                {workKnown && (
                  <>
                    <td className="py-1.5 pr-3 text-right tabular-nums">{durationText(column.work_ms)}</td>
                    <td className="py-1.5 pr-3 text-right tabular-nums text-muted-foreground">
                      {durationText(column.waiting_ms)}
                    </td>
                    <td className="py-1.5">
                      <ShareBar share={workShare(column)} />
                    </td>
                  </>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function Timeline({ visits, workKnown }: { visits: Visit[]; workKnown: boolean }) {
  return (
    <Collapsible className="rounded-md border">
      <CollapsibleTrigger className="group flex w-full items-center gap-2 px-3 py-2 text-left text-xs font-medium">
        <ChevronRight
          aria-hidden
          className="h-3.5 w-3.5 shrink-0 transition-transform group-data-[state=open]:rotate-90"
        />
        Timeline ({count(visits.length, "visit", "visits")})
      </CollapsibleTrigger>
      <CollapsibleContent className="border-t px-3 py-2">
        <ol className="space-y-1.5 text-xs">
          {visits.map((visit, i) => (
            <li
              key={i}
              className={cn(
                "flex flex-wrap items-center gap-x-3 gap-y-0.5",
                visit.kind !== "column" && "text-muted-foreground",
              )}
            >
              <span className="tabular-nums text-muted-foreground">{formatDateTime(visit.entered_at)}</span>
              <span className="font-medium">{visitLabel(visit)}</span>
              <span className="tabular-nums">{durationText(visit.elapsed_ms)}</span>
              {workKnown && (
                <span className="tabular-nums text-muted-foreground">
                  {durationText(visit.work_ms)} work · {durationText(visit.waiting_ms)} waiting
                </span>
              )}
              {visit.left_at === null && <Badge variant="info">now</Badge>}
            </li>
          ))}
        </ol>
      </CollapsibleContent>
    </Collapsible>
  );
}
