import { Check, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { TableSkeleton } from "@/components/loading-skeleton";
import { EmptyState } from "@/components/empty-state";
import { usePendingPairings, useApprovePairing, useRejectPairing } from "@/hooks/use-pairing";

interface PendingItem {
  code: string;
  device_name?: string;
  scopes?: string[];
  created_at?: string;
}

function formatTs(ts?: string): string {
  if (!ts) return "—";
  try {
    const diff = Date.now() - new Date(ts).getTime();
    const secs = Math.floor(diff / 1000);
    if (secs < 60) return "just now";
    const mins = Math.floor(secs / 60);
    if (mins < 60) return `${mins}m ago`;
    const hrs = Math.floor(mins / 60);
    if (hrs < 24) return `${hrs}h ago`;
    const days = Math.floor(hrs / 24);
    return `${days}d ago`;
  } catch {
    return ts;
  }
}

export function PendingRequests() {
  const { data, isLoading } = usePendingPairings();
  const approveMutation = useApprovePairing();
  const rejectMutation = useRejectPairing();

  if (isLoading) return <TableSkeleton rows={3} />;

  const pending: PendingItem[] = [
    ...((data?.channel ?? []) as PendingItem[]),
    ...((data?.device ?? []) as PendingItem[]),
  ];

  if (pending.length === 0) {
    return (
      <EmptyState
        icon={Check}
        title="No Pending Requests"
        description="All pairing requests have been processed."
      />
    );
  }

  return (
    <div className="border rounded-lg overflow-hidden">
      <table className="w-full">
        <thead>
          <tr className="border-b bg-muted/50">
            <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
              Code
            </th>
            <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
              Device Name
            </th>
            <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 hidden md:table-cell">
              Requested
            </th>
            <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-40">
              Actions
            </th>
          </tr>
        </thead>
        <tbody>
          {pending.map((item) => (
            <tr
              key={item.code}
              className="border-b last:border-b-0 hover:bg-muted/20 transition-colors"
            >
              <td className="px-4 py-3">
                <span className="text-sm font-mono">{item.code}</span>
              </td>
              <td className="px-4 py-3">
                <span className="text-sm">{item.device_name ?? "—"}</span>
              </td>
              <td className="px-4 py-3 hidden md:table-cell">
                <span className="text-xs text-muted-foreground">
                  {formatTs(item.created_at)}
                </span>
              </td>
              <td className="px-4 py-3 text-right">
                <div className="flex items-center justify-end gap-1">
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-7 px-2 text-xs"
                    disabled={approveMutation.isPending}
                    onClick={() => approveMutation.mutate(item.code)}
                  >
                    <Check className="h-3.5 w-3.5 mr-1" />
                    Approve
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-7 px-2 text-xs text-destructive hover:text-destructive"
                    disabled={rejectMutation.isPending}
                    onClick={() => rejectMutation.mutate(item.code)}
                  >
                    <X className="h-3.5 w-3.5 mr-1" />
                    Reject
                  </Button>
                </div>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
