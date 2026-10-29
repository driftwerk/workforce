import { useState } from "react";
import { Monitor, ShieldOff } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { TableSkeleton } from "@/components/loading-skeleton";
import { EmptyState } from "@/components/empty-state";
import { usePairedDevices, useRevokeDevice } from "@/hooks/use-pairing";

export interface PairedDevice {
  id: string;
  device_name: string;
  client_id?: string;
  scopes: string[];
  created_at: string;
  last_used?: string;
  revoked: boolean;
  /** The person the device belongs to; null when it acts anonymously. */
  owner?: { id: string; display_name: string; email?: string | null } | null;
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

function isRecentlyActive(lastUsed?: string): boolean {
  if (!lastUsed) return false;
  const diff = Date.now() - new Date(lastUsed).getTime();
  return diff < 30 * 60 * 1000; // within last 30 minutes
}

export function DeviceList() {
  const { data, isLoading } = usePairedDevices();
  const revokeMutation = useRevokeDevice();
  const [revokeTarget, setRevokeTarget] = useState<PairedDevice | null>(null);

  if (isLoading) return <TableSkeleton rows={3} />;

  const devices = (data?.devices ?? []) as PairedDevice[];
  const active = devices.filter((d) => !d.revoked);
  const revoked = devices.filter((d) => d.revoked);

  if (devices.length === 0) {
    return (
      <EmptyState
        icon={Monitor}
        title="No Devices"
        description="No approved devices found."
      />
    );
  }

  const renderRow = (device: PairedDevice) => {
    const active = isRecentlyActive(device.last_used);
    return (
      <tr
        key={device.id}
        className={`border-b last:border-b-0 hover:bg-muted/20 transition-colors ${device.revoked ? "opacity-50" : ""}`}
      >
        <td className="px-4 py-3">
          <div className="flex items-center gap-2">
            <span
              className={`h-2 w-2 rounded-full shrink-0 ${active ? "bg-green-500" : "bg-muted-foreground/30"}`}
              title={active ? "Active now" : "Inactive"}
            />
            <span className="text-sm font-medium">
              {device.device_name || "Unnamed device"}
            </span>
          </div>
          <span className="block pl-4 text-xs text-muted-foreground">
            {device.owner
              ? device.owner.email ?? device.owner.display_name
              : "Not connected to an account"}
          </span>
        </td>
        <td className="px-4 py-3">
          <span className="text-xs font-mono text-muted-foreground truncate max-w-[140px] block">
            {device.id.slice(0, 8)}...
          </span>
        </td>
        <td className="px-4 py-3 hidden sm:table-cell">
          <span className="text-xs text-muted-foreground">
            {formatTs(device.created_at)}
          </span>
        </td>
        <td className="px-4 py-3 hidden md:table-cell">
          <span className={`text-xs ${active ? "text-green-600 font-medium" : "text-muted-foreground"}`}>
            {formatTs(device.last_used)}
          </span>
        </td>
        <td className="px-4 py-3">
          {device.revoked ? (
            <Badge variant="destructive" className="text-xs">Revoked</Badge>
          ) : (
            <Badge variant="outline" className="text-xs">Active</Badge>
          )}
        </td>
        <td className="px-4 py-3 text-right">
          {!device.revoked && (
            <Button
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-xs text-destructive hover:text-destructive"
              onClick={() => setRevokeTarget(device)}
            >
              <ShieldOff className="h-3.5 w-3.5 mr-1" />
              Revoke
            </Button>
          )}
        </td>
      </tr>
    );
  };

  return (
    <>
      <div className="border rounded-lg overflow-hidden">
        <table className="w-full">
          <thead>
            <tr className="border-b bg-muted/50">
              <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                Device
              </th>
              <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                ID
              </th>
              <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 hidden sm:table-cell">
                Paired
              </th>
              <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 hidden md:table-cell">
                Last Used
              </th>
              <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                Status
              </th>
              <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-28">
                Actions
              </th>
            </tr>
          </thead>
          <tbody>
            {active.map(renderRow)}
            {revoked.map(renderRow)}
          </tbody>
        </table>
      </div>

      <Dialog open={!!revokeTarget} onOpenChange={() => setRevokeTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Revoke Device</DialogTitle>
            <DialogDescription>
              Are you sure you want to revoke access for{" "}
              <span className="font-semibold">
                {revokeTarget?.device_name ?? revokeTarget?.id}
              </span>
              ? The device will need to re-pair.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setRevokeTarget(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={revokeMutation.isPending}
              onClick={() => {
                if (revokeTarget) {
                  revokeMutation.mutate(revokeTarget.id, {
                    onSuccess: () => setRevokeTarget(null),
                  });
                }
              }}
            >
              {revokeMutation.isPending ? "Revoking..." : "Revoke"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
