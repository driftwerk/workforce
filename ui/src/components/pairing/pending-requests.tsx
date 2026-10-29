import { useState } from "react";
import { Check, UserCheck, X } from "lucide-react";
import { Button } from "@/components/ui/button";
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
import { EntityCombobox } from "@/components/permissions/entity-picker";
import {
  usePendingPairings,
  useApprovePairing,
  useRejectPairing,
  useApproveInto,
} from "@/hooks/use-pairing";
import { deviceKindLabel, pairErrorMessage } from "@/lib/pairing";

interface PendingItem {
  code: string;
  device_name?: string;
  scopes?: string[];
  created_at?: string;
  /** Present on device requests only; channel requests have neither. */
  device_kind?: string | null;
  identity_hint?: string | null;
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

function ApproveIntoDialog({
  target,
  onClose,
}: {
  target: PendingItem | null;
  onClose: () => void;
}) {
  const [identityId, setIdentityId] = useState("");
  const [label, setLabel] = useState("");
  const approveInto = useApproveInto();

  const close = () => {
    setIdentityId("");
    setLabel("");
    approveInto.reset();
    onClose();
  };

  return (
    <Dialog open={!!target} onOpenChange={(open) => !open && close()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Approve into a person</DialogTitle>
          <DialogDescription>
            Connect <strong>{target?.device_name ?? "this device"}</strong> (code{" "}
            <span className="font-mono">{target?.code}</span>) to the person you choose. The
            device will act as them.
          </DialogDescription>
        </DialogHeader>
        <EntityCombobox
          kind="user"
          value={identityId}
          onChange={(id, l) => {
            setIdentityId(id);
            setLabel(l);
          }}
          surface="dialog"
        />
        {approveInto.isError && (
          <p className="text-sm text-destructive" role="alert">
            {pairErrorMessage(approveInto.error)}
          </p>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={close}>
            Cancel
          </Button>
          <Button
            disabled={!identityId || !target || approveInto.isPending}
            onClick={() =>
              target &&
              approveInto.mutate({ code: target.code, identityId }, { onSuccess: close })
            }
          >
            {label ? `Approve into ${label}` : "Approve"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export function PendingRequests() {
  const { data, isLoading } = usePendingPairings();
  const approveMutation = useApprovePairing();
  const rejectMutation = useRejectPairing();
  const [assignTarget, setAssignTarget] = useState<PendingItem | null>(null);

  if (isLoading) return <TableSkeleton rows={3} />;

  const devices = (data?.device ?? []) as PendingItem[];
  const deviceCodes = new Set(devices.map((d) => d.code));
  const pending: PendingItem[] = [...((data?.channel ?? []) as PendingItem[]), ...devices];

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
    <>
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
              <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-64">
                Actions
              </th>
            </tr>
          </thead>
          <tbody>
            {pending.map((item) => {
              // Only a device request (not a channel one) can go into a person.
              const isDevice = deviceCodes.has(item.code);
              return (
                <tr
                  key={item.code}
                  className="border-b last:border-b-0 hover:bg-muted/20 transition-colors"
                >
                  <td className="px-4 py-3">
                    <span className="text-sm font-mono">{item.code}</span>
                  </td>
                  <td className="px-4 py-3">
                    <div className="text-sm">{item.device_name ?? "—"}</div>
                    {isDevice && (
                      <div className="text-xs text-muted-foreground">
                        {deviceKindLabel(item.device_kind)}
                        {item.identity_hint && (
                          <span title="The device said this itself; nothing has checked it.">
                            {" "}
                            · claims to be {item.identity_hint} (unverified)
                          </span>
                        )}
                      </div>
                    )}
                  </td>
                  <td className="px-4 py-3 hidden md:table-cell">
                    <span className="text-xs text-muted-foreground">
                      {formatTs(item.created_at)}
                    </span>
                  </td>
                  <td className="px-4 py-3 text-right">
                    <div className="flex items-center justify-end gap-1">
                      {isDevice && (
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-7 px-2 text-xs"
                          onClick={() => setAssignTarget(item)}
                        >
                          <UserCheck className="h-3.5 w-3.5 mr-1" />
                          Approve into…
                        </Button>
                      )}
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
              );
            })}
          </tbody>
        </table>
      </div>
      <ApproveIntoDialog target={assignTarget} onClose={() => setAssignTarget(null)} />
    </>
  );
}
