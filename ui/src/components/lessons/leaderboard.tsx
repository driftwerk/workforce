import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import type { LeaderboardEntry } from "@/lib/api/lessons";

function truncate(s: string, n = 120): string {
  return s.length > n ? `${s.slice(0, n)}…` : s;
}

export function Leaderboard({
  entries,
  isLoading,
}: {
  entries: LeaderboardEntry[];
  isLoading?: boolean;
}) {
  const applied = entries.filter((e) => e.applied > 0);
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Most-applied lessons</CardTitle>
        <p className="text-sm text-muted-foreground">
          The lessons that have steered the most turns.
        </p>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            Loading…
          </div>
        ) : applied.length === 0 ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            No lessons have been applied yet.
          </div>
        ) : (
          <ol className="space-y-2">
            {applied.map((e, i) => (
              <li key={e.id} className="flex items-start gap-3 text-sm">
                <span className="w-5 shrink-0 text-right font-mono text-muted-foreground">
                  {i + 1}
                </span>
                <span className="flex-1 text-foreground/90" title={e.content}>
                  {truncate(e.content)}
                </span>
                <Badge variant="success" className="shrink-0">
                  ×{e.applied}
                </Badge>
                <span className="w-12 shrink-0 text-right font-mono text-xs text-muted-foreground">
                  {e.confidence.toFixed(2)}
                </span>
              </li>
            ))}
          </ol>
        )}
      </CardContent>
    </Card>
  );
}
