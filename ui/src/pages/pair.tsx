import { useState, type ReactNode } from "react";
import { useSearchParams } from "react-router";
import { useMutation, useQuery } from "@tanstack/react-query";
import { CheckCircle2, Loader2, Smartphone, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { pairingApi, type PairedIdentity, type PendingDevice } from "@/lib/api/pairing";
import { accountLine, deviceKindLabel, pairErrorMessage } from "@/lib/pairing";

/** Everything the `/pair` card can show, decided outside the markup. */
export type PairState =
  | { kind: "no-code" }
  | { kind: "loading" }
  | { kind: "error"; error: unknown }
  | {
      kind: "ready";
      device: PendingDevice;
      busy: "approve" | "reject" | null;
      failure: unknown;
    }
  | { kind: "approved"; identity: PairedIdentity }
  | { kind: "rejected" };

function formatWhen(ts: string): string {
  const when = new Date(ts);
  return Number.isNaN(when.getTime()) ? ts : when.toLocaleString();
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-md">
        <CardHeader className="text-center">
          <div className="mx-auto mb-2 flex h-12 w-12 items-center justify-center rounded-full bg-primary/10">
            <Smartphone className="h-6 w-6 text-primary" />
          </div>
          <CardTitle>Connect a device</CardTitle>
        </CardHeader>
        <CardContent>{children}</CardContent>
      </Card>
    </div>
  );
}

function Notice({ tone, children }: { tone: "error" | "info"; children: ReactNode }) {
  return (
    <p
      className={tone === "error" ? "text-sm text-destructive text-center" : "text-sm text-center"}
      role={tone === "error" ? "alert" : "status"}
    >
      {children}
    </p>
  );
}

/**
 * The `/pair` card for one state. Shows only what the device itself said
 * (its name and kind) — never anything about who asked for the code.
 */
export function PairView({
  state,
  onApprove,
  onReject,
}: {
  state: PairState;
  onApprove: () => void;
  onReject: () => void;
}) {
  switch (state.kind) {
    case "no-code":
      return (
        <Shell>
          <Notice tone="error">
            This link has no pairing code. Open the link your device showed you, or ask your
            device for a new code.
          </Notice>
        </Shell>
      );
    case "loading":
      return (
        <Shell>
          <div className="flex justify-center py-6">
            <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
          </div>
        </Shell>
      );
    case "error":
      return (
        <Shell>
          <Notice tone="error">{pairErrorMessage(state.error)}</Notice>
        </Shell>
      );
    case "approved":
      return (
        <Shell>
          <div className="flex flex-col items-center gap-3">
            <CheckCircle2 className="h-8 w-8 text-green-600" />
            <Notice tone="info">
              Connected to {accountLine(state.identity)}. You can return to your device.
            </Notice>
          </div>
        </Shell>
      );
    case "rejected":
      return (
        <Shell>
          <div className="flex flex-col items-center gap-3">
            <XCircle className="h-8 w-8 text-muted-foreground" />
            <Notice tone="info">The device was not connected. You can close this page.</Notice>
          </div>
        </Shell>
      );
    case "ready": {
      const { device, busy, failure } = state;
      return (
        <Shell>
          <div className="space-y-4">
            <CardDescription className="text-center text-base text-foreground">
              Connect <strong>{device.device_name}</strong> to your account?
            </CardDescription>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
              <dt className="text-muted-foreground">Device</dt>
              <dd>{device.device_name}</dd>
              <dt className="text-muted-foreground">Kind</dt>
              <dd>{deviceKindLabel(device.device_kind)}</dd>
              <dt className="text-muted-foreground">Requested</dt>
              <dd>{formatWhen(device.requested_at)}</dd>
              <dt className="text-muted-foreground">Code</dt>
              <dd className="font-mono">{device.code}</dd>
            </dl>
            <p className="text-xs text-muted-foreground">
              Only approve if this code is showing on a device in front of you. Once connected,
              it acts as you.
            </p>
            {failure != null && <Notice tone="error">{pairErrorMessage(failure)}</Notice>}
            <div className="flex gap-2">
              <Button className="flex-1" disabled={busy !== null} onClick={onApprove}>
                {busy === "approve" ? <Loader2 className="h-4 w-4 animate-spin" /> : "Approve"}
              </Button>
              <Button
                className="flex-1"
                variant="outline"
                disabled={busy !== null}
                onClick={onReject}
              >
                {busy === "reject" ? <Loader2 className="h-4 w-4 animate-spin" /> : "Reject"}
              </Button>
            </div>
          </div>
        </Shell>
      );
    }
  }
}

/**
 * `/pair?code=…` — a device showed its person this link; the signed-in person
 * confirms and the device joins *their* account.
 */
export default function PairPage() {
  const [params] = useSearchParams();
  const code = (params.get("code") ?? "").trim();
  const [done, setDone] = useState<PairState | null>(null);

  const pending = useQuery({
    queryKey: ["pair-device-pending", code],
    queryFn: () => pairingApi.devicePending(code),
    enabled: !!code,
    retry: false,
  });
  const approve = useMutation({
    mutationFn: () => pairingApi.approveSelf(code),
    onSuccess: (res) => setDone({ kind: "approved", identity: res.identity }),
  });
  const reject = useMutation({
    mutationFn: () => pairingApi.rejectSelf(code),
    onSuccess: () => setDone({ kind: "rejected" }),
  });

  let state: PairState;
  if (!code) state = { kind: "no-code" };
  else if (done) state = done;
  else if (pending.isLoading) state = { kind: "loading" };
  else if (pending.isError || !pending.data) state = { kind: "error", error: pending.error };
  else
    state = {
      kind: "ready",
      device: pending.data,
      busy: approve.isPending ? "approve" : reject.isPending ? "reject" : null,
      failure: approve.error ?? reject.error ?? null,
    };

  return (
    <PairView state={state} onApprove={() => approve.mutate()} onReject={() => reject.mutate()} />
  );
}
