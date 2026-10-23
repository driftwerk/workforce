import { Link } from "react-router";
import { Workflow, AlertCircle } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { usePipelineCatalogue } from "@/hooks/use-pipeline";
import type { PipelineSummary } from "@/lib/api/pipelines";
import { formatRelativeTime } from "@/lib/utils";

function scopeLabel(scope: string): string {
  if (scope === "global") return "Global";
  const [kind, id] = scope.split(":", 2);
  return kind === "agent" ? `Agent ${id}` : kind === "user" ? "Mine" : scope;
}

function PipelineCard({ p }: { p: PipelineSummary }) {
  const inFlow = p.lanes.reduce((n, l) => n + (p.counts.columns[l.id] ?? 0), 0);
  const parked = p.exits.reduce((n, e) => n + (p.counts.columns[e.id] ?? 0), 0);
  const last = p.counts.last_moved_at ?? null;
  return (
    <Link to={`/pipelines/${encodeURIComponent(p.scope)}/${encodeURIComponent(p.pipeline)}`} className="block focus:outline-none focus-visible:ring-2 focus-visible:ring-ring rounded-lg">
      <Card className="h-full transition-colors hover:border-primary/40">
        <CardContent className="space-y-3 p-4">
          <div className="flex items-start justify-between gap-3">
            <div>
              <h2 className="text-base font-semibold">{p.name}</h2>
              <p className="text-xs text-muted-foreground">{scopeLabel(p.scope)} · {p.counts.items} {p.item_label}{p.counts.items === 1 ? "" : "s"}</p>
            </div>
            <span className="shrink-0 text-[11px] text-muted-foreground">{last ? `moved ${formatRelativeTime(last)}` : "no movement yet"}</span>
          </div>
          {p.description && <p className="line-clamp-2 text-sm text-muted-foreground">{p.description}</p>}
          <ol className="flex gap-1" aria-label="items per lane">
            {p.lanes.map((l) => {
              const n = p.counts.columns[l.id] ?? 0;
              return (
                <li key={l.id} className="flex min-w-0 flex-1 flex-col items-center gap-1" title={`${l.label}: ${n}`}>
                  <div className="flex h-8 w-full items-end rounded-sm bg-muted">
                    <div
                      className="w-full rounded-sm bg-primary/70"
                      style={{ height: inFlow ? `${Math.max(n ? 12 : 0, (n / Math.max(1, inFlow)) * 100)}%` : "0%" }}
                    />
                  </div>
                  <span className="w-full truncate text-center text-[10px] text-muted-foreground">{l.label}</span>
                </li>
              );
            })}
          </ol>
          <div className="flex gap-4 text-[11px] text-muted-foreground">
            <span>{inFlow} in flow</span>
            <span>{parked} left the flow</span>
            {p.counts.unplaced > 0 && <span className="text-warning">{p.counts.unplaced} unplaced</span>}
          </div>
        </CardContent>
      </Card>
    </Link>
  );
}

export default function PipelinesPage() {
  const catalogue = usePipelineCatalogue();
  if (catalogue.isLoading && !catalogue.data) return <PageSkeleton />;
  const status = (catalogue.error as { status?: number } | null)?.status;
  const pipelines = catalogue.data?.pipelines ?? [];

  return (
    <div className="space-y-5 p-6">
      <div>
        <h1 className="text-2xl font-bold">Pipelines</h1>
        <p className="text-muted-foreground">Items moving through stages, recorded by your workflows and skills.</p>
      </div>
      {catalogue.error ? (
        <EmptyState
          icon={AlertCircle}
          title={status === 503 ? "The document store is not configured on this hub" : "Could not load pipelines"}
          description={
            status === 503
              ? "Pipelines live in the hub's document store. Enable [postgres] on the hub to keep them."
              : String((catalogue.error as Error).message ?? catalogue.error)
          }
          action={{ label: "Retry", onClick: () => catalogue.refetch() }}
        />
      ) : pipelines.length === 0 ? (
        <EmptyState
          icon={Workflow}
          title="No pipelines yet"
          description="A pipeline declares itself: ask an agent to define one for a workflow (“define a pipeline for the new-starter workflow”), or call pipeline.define. Once its workflow records a move, it appears here."
        />
      ) : (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {pipelines.map((p) => (
            <PipelineCard key={`${p.scope}/${p.pipeline}`} p={p} />
          ))}
        </div>
      )}
    </div>
  );
}
