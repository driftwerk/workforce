import { useCallback, useState } from "react";
import { Check, Copy, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { TriggerUrl } from "@/lib/api/workflows";
import { triggerAddressList, type TriggerAddresses } from "@/lib/workflows/trigger-urls";

/**
 * Where each of a workflow's HTTP triggers answers, for someone configuring
 * the system that will call it. The minted address comes first because it is
 * the one to hand out; the older owner-derived address is shown beneath it so
 * a year-old integration is recognisable, and only while it still answers.
 */
export function TriggerUrlsPanel({ triggers }: { triggers: TriggerUrl[] | undefined }) {
  return (
    <div className="p-4 space-y-4 overflow-auto h-full">
      <p className="text-xs text-muted-foreground max-w-2xl">
        Each HTTP trigger answers at the addresses below; callers{" "}
        <code className="font-mono">POST</code> to them with the trigger's authentication. A minted
        address — the <code className="font-mono">/triggers/t.…</code> form — is not derived from
        who owns this workflow, so it keeps answering if the workflow is handed to someone else.
      </p>
      {triggerAddressList(triggers).map((trigger) => (
        <TriggerRow key={trigger.triggerId} trigger={trigger} />
      ))}
    </div>
  );
}

function TriggerRow({ trigger }: { trigger: TriggerAddresses }) {
  return (
    <div className="rounded-md border p-3 space-y-2">
      <div className="text-xs font-medium">{trigger.triggerId}</div>
      {trigger.primary ? (
        <Address url={trigger.primary} />
      ) : (
        <p className="text-xs text-muted-foreground">
          No address: another workflow claims this trigger id, so the endpoint is disabled. Rename
          one of them to bring it back.
        </p>
      )}
      {trigger.alsoAnswers && (
        <div className="space-y-1">
          <div className="text-[10px] uppercase tracking-wide text-muted-foreground">
            also answers — existing callers, unchanged
          </div>
          <Address url={trigger.alsoAnswers} />
        </div>
      )}
    </div>
  );
}

type CopyState = "idle" | "copied" | "failed";

function Address({ url }: { url: string }) {
  const [state, setState] = useState<CopyState>("idle");
  // A hub reached over plain HTTP has no clipboard API, and a denied permission
  // rejects: either way the address is still selectable, so say which it is
  // rather than leaving a click that looks like it worked.
  const copy = useCallback(() => {
    const write = navigator.clipboard?.writeText(url) ?? Promise.reject(new Error("unavailable"));
    write
      .then(() => setState("copied"))
      .catch(() => setState("failed"))
      .finally(() => setTimeout(() => setState("idle"), 2000));
  }, [url]);

  const label = state === "failed" ? `Select and copy ${url}` : `Copy ${url}`;
  return (
    <div className="flex items-center gap-2">
      <code
        title={url}
        className="flex-1 min-w-0 truncate rounded bg-muted px-2 py-1 font-mono text-xs"
      >
        {url}
      </code>
      <Button
        variant="ghost"
        size="icon"
        className={cn("h-7 w-7 shrink-0", state === "failed" && "text-destructive")}
        onClick={copy}
        aria-label={label}
        title={label}
      >
        {state === "copied" ? (
          <Check className="h-3.5 w-3.5" />
        ) : state === "failed" ? (
          <X className="h-3.5 w-3.5" />
        ) : (
          <Copy className="h-3.5 w-3.5" />
        )}
      </Button>
    </div>
  );
}
