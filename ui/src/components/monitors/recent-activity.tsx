import { Badge } from "@/components/ui/badge";
import { useMonitorActivity } from "@/hooks/use-monitors";

interface RecentActivityProps {
  id: string;
  owner?: string | null;
}

/** How the engine marks a dispatch its decide gate turned away (`MonitorEngine::run_and_settle`). */
const SKIPPED_PREFIX = "skipped:";

function statusBadge(row: { completed_at?: string | null; error?: string | null }) {
  if (row.error && !row.completed_at) return <Badge variant="warning">Retrying</Badge>;
  if (row.error?.startsWith(SKIPPED_PREFIX)) return <Badge variant="outline">Skipped</Badge>;
  if (row.error) return <Badge variant="destructive">Failed</Badge>;
  if (row.completed_at) return <Badge variant="secondary">Done</Badge>;
  return <Badge variant="outline">Pending</Badge>;
}

export function RecentActivity({ id, owner }: RecentActivityProps) {
  const { data, isLoading } = useMonitorActivity(id, owner);
  const rows = data?.activity ?? [];

  return (
    <div className="p-4">
      <h2 className="text-sm font-semibold mb-2">Recent activity</h2>
      {isLoading ? (
        <p className="text-xs text-muted-foreground">Loading…</p>
      ) : rows.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          No dispatches yet. Matching messages and observations will appear here once they arrive.
        </p>
      ) : (
        <div className="border rounded-md divide-y">
          {rows.map((r) => (
            <div key={r.message_id} className="flex items-center gap-3 px-3 py-2 text-xs">
              <span className="shrink-0">{statusBadge(r)}</span>
              <span className="flex-1 min-w-0 truncate font-mono" title={r.error ?? undefined}>
                {r.text || "(no text)"}
              </span>
              <span className="shrink-0 text-muted-foreground">
                {new Date(r.claimed_at).toLocaleString()}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
