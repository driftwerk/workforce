import { AlertTriangle } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import type { RecurrenceCluster } from "@/lib/api/lessons";

function truncate(s: string, n = 140): string {
  return s.length > n ? `${s.slice(0, n)}…` : s;
}

export function RecurrencesList({
  clusters,
  isLoading,
}: {
  clusters: RecurrenceCluster[];
  isLoading?: boolean;
}) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base flex items-center gap-2">
          <AlertTriangle className="h-4 w-4 text-warning" />
          Cross-day recurrences
        </CardTitle>
        <p className="text-sm text-muted-foreground">
          Triggers that surfaced the same mistake on more than one day.
        </p>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            Loading…
          </div>
        ) : clusters.length === 0 ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            No cross-day recurrences — no trigger has repeated across days.
          </div>
        ) : (
          <ul className="space-y-4">
            {clusters.map((c) => (
              <li
                key={c.trigger}
                className="rounded-md border border-border bg-muted/20 p-3"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="warning" className="font-mono">
                    {c.trigger}
                  </Badge>
                  <span className="text-xs text-muted-foreground">
                    {c.dates.length} day{c.dates.length !== 1 ? "s" : ""}:
                  </span>
                  {c.dates.map((d) => (
                    <span
                      key={d}
                      className="text-xs font-mono text-muted-foreground"
                    >
                      {d}
                    </span>
                  ))}
                </div>
                <ul className="mt-2 space-y-1.5">
                  {c.lessons.map((l) => (
                    <li
                      key={l.id}
                      className="flex items-start gap-2 text-sm"
                    >
                      <span className="font-mono text-xs text-muted-foreground whitespace-nowrap">
                        {l.date}
                      </span>
                      {l.applied > 0 ? (
                        <Badge variant="success" className="shrink-0">
                          applied ×{l.applied}
                        </Badge>
                      ) : (
                        <Badge variant="outline" className="shrink-0">
                          never applied
                        </Badge>
                      )}
                      <span className="text-foreground/90" title={l.content}>
                        {truncate(l.content)}
                      </span>
                    </li>
                  ))}
                </ul>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}
