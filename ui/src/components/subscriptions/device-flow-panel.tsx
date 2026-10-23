import { useEffect, useState } from "react";
import { Check, Copy, ExternalLink, Loader2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useNow } from "@/hooks/use-now";
import { useCancelDeviceFlow } from "@/hooks/use-subscriptions";
import type { DeviceFlowView } from "@/lib/api/subscriptions";
import { apiErrorMessage } from "@/lib/api-error";
import { countdownLabel, nextFlowUi, secondsLeft, type PendingFlow } from "@/lib/subscriptions/device-flow";
import { accountLabel } from "@/lib/subscriptions/status";

function PendingCode({ flow }: { flow: PendingFlow }) {
  const now = useNow(true, 1000);
  const cancel = useCancelDeviceFlow();
  const [copied, setCopied] = useState<"copied" | "failed" | null>(null);
  const left = secondsLeft(flow, now);
  const minutesLeft = Math.ceil(left / 60);

  useEffect(() => {
    if (copied !== "copied") return;
    const timer = window.setTimeout(() => setCopied(null), 1500);
    return () => window.clearTimeout(timer);
  }, [copied]);

  if (left === 0) {
    return (
      <p role="status" className="text-sm text-destructive">
        The code expired before it was used.
      </p>
    );
  }

  function copy() {
    Promise.resolve()
      .then(() => navigator.clipboard.writeText(flow.user_code))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  }

  return (
    <div className="space-y-3 rounded-md border p-3">
      <p className="text-sm">
        Open{" "}
        <a
          href={flow.verification_url}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-1 font-medium underline"
        >
          {flow.verification_url}
          <ExternalLink className="h-3 w-3" />
        </a>{" "}
        and enter this code:
      </p>
      <div className="flex items-center gap-2">
        <output aria-label="One-time code" className="rounded bg-muted px-3 py-1.5 font-mono text-lg tracking-widest">
          {flow.user_code}
        </output>
        <Button type="button" size="sm" variant="outline" onClick={copy}>
          {copied === "copied" ? <Check className="h-4 w-4 mr-1" /> : <Copy className="h-4 w-4 mr-1" />}
          {copied === "copied" ? "Copied" : "Copy"}
        </Button>
      </div>
      <p role="status" className="text-xs text-destructive empty:hidden">
        {copied === "failed" && "Couldn't copy the code. Select it and copy it by hand."}
      </p>
      <p className="text-xs text-muted-foreground">
        Only enter this code if you started this sign-in. If a website or another person gave you
        this code, ignore it.
      </p>
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs text-muted-foreground">
          <span aria-hidden="true">Expires in {countdownLabel(left)}</span>
          <span className="sr-only" aria-live="polite">
            {`Code expires in ${minutesLeft} minute${minutesLeft === 1 ? "" : "s"}`}
          </span>
        </p>
        <Button
          type="button"
          size="sm"
          variant="ghost"
          onClick={() => cancel.mutate(flow.flow_id)}
          disabled={cancel.isPending}
        >
          {cancel.isPending ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <X className="h-4 w-4 mr-1" />}
          Cancel
        </Button>
      </div>
      {cancel.error && <p className="text-sm text-destructive">{apiErrorMessage(cancel.error)}</p>}
    </div>
  );
}

/** Where a ChatGPT sign-in stands: its code while it waits, then how it
 *  ended. A settled sign-in never shows its code. */
export function DeviceFlowPanel({ flow, then }: { flow: DeviceFlowView; then?: string }) {
  const ui = nextFlowUi(flow, false);
  if (ui.pending) return <PendingCode flow={ui.pending} />;
  if (ui.connected) {
    const account = accountLabel(ui.connected);
    return (
      <p role="status" className="text-sm text-success">
        {[account ? `Connected · ${account}` : "Connected", then].filter(Boolean).join(". ")}
      </p>
    );
  }
  return (
    <p role="status" className="text-sm text-destructive">
      {ui.failure}
    </p>
  );
}
