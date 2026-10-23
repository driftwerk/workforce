import { useState, type ReactNode } from "react";
import { Link2, Loader2, Save, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { InlineCode } from "@/components/ui/inline-code";
import { SecretInput } from "@/components/subscriptions/secret-input";
import { DeviceFlowPanel } from "@/components/subscriptions/device-flow-panel";
import {
  useCurrentDeviceFlow,
  useDeviceFlow,
  useRemoveLogin,
  useSaveLogin,
  useStartDeviceFlow,
} from "@/hooks/use-subscriptions";
import type { DeviceTarget, LoginTarget, SubscriptionKind } from "@/lib/api/subscriptions";
import { apiErrorMessage } from "@/lib/api-error";
import { connectView, flowTargetLabel, nextFlowUi, watchedFlow } from "@/lib/subscriptions/device-flow";
import { KIND_LABELS } from "@/lib/subscriptions/status";

export const PASTE_HINTS: Record<SubscriptionKind, { label: string; placeholder: string; hint: ReactNode }> = {
  claude: {
    label: "Claude setup token",
    placeholder: "sk-ant-oat…",
    hint: (
      <>
        Run <InlineCode>claude setup-token</InlineCode> and paste the <InlineCode>sk-ant-oat…</InlineCode>{" "}
        token it prints.
      </>
    ),
  },
  chatgpt: {
    label: "ChatGPT auth.json",
    placeholder: "Contents of auth.json",
    hint: (
      <>
        Run <InlineCode>codex login --device-auth</InlineCode> with <InlineCode>CODEX_HOME</InlineCode> set
        to an empty folder, then paste that folder's <InlineCode>auth.json</InlineCode>. Never paste a file
        another Codex keeps using: a sign-in works in one place only.
      </>
    ),
  },
};

function PasteLogin({ kind, target }: { kind: SubscriptionKind; target: LoginTarget }) {
  const [value, setValue] = useState("");
  const save = useSaveLogin();
  const { label, placeholder, hint } = PASTE_HINTS[kind];
  return (
    <div className="space-y-2">
      <p className="text-xs text-muted-foreground">{hint}</p>
      <SecretInput value={value} onChange={setValue} placeholder={placeholder} aria-label={label} />
      <Button
        size="sm"
        onClick={() => save.mutate({ kind, target, value }, { onSuccess: () => setValue("") })}
        disabled={!value.trim() || save.isPending}
      >
        {save.isPending ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <Save className="h-4 w-4 mr-1" />}
        Save
      </Button>
      {save.error && <p className="text-sm text-destructive">{apiErrorMessage(save.error)}</p>}
    </div>
  );
}

/** A ChatGPT sign-in for `target`. The caller keeps one sign-in at a time,
 *  so starting another first warns that it replaces the one waiting. */
function DeviceConnect({ target }: { target: DeviceTarget }) {
  const start = useStartDeviceFlow(target);
  const kept = useCurrentDeviceFlow();
  const latest = kept.data ?? null;
  const pendingId = latest?.state === "pending" ? latest.flow_id : null;
  const polled = useDeviceFlow(pendingId);
  const [watched, setWatched] = useState<string | null>(null);
  if (pendingId && pendingId !== watched) setWatched(pendingId);
  const view = connectView(watchedFlow(latest, polled.data, watched), target, start.isPending);
  const ui = nextFlowUi(view.shown, start.isPending);
  const [confirming, setConfirming] = useState(false);
  const shownId = view.shown?.flow_id ?? null;
  const [paste, setPaste] = useState<{ open: boolean; flowId: string | null } | null>(null);
  const pasteOpen = paste?.flowId === shownId ? paste.open : ui.showPaste;

  function begin() {
    setConfirming(false);
    start.mutate(undefined, { onSuccess: (flow) => setWatched(flow.flow_id) });
  }

  return (
    <div className="space-y-3">
      <Button
        size="sm"
        onClick={() => (view.replaces ? setConfirming(true) : begin())}
        disabled={!view.canStart || confirming}
      >
        {start.isPending ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <Link2 className="h-4 w-4 mr-1" />}
        Connect ChatGPT
      </Button>
      {confirming && view.replaces && (
        <div role="alert" className="space-y-2 rounded-md border border-warning/40 bg-warning/10 p-3 text-sm">
          <p>
            A ChatGPT sign-in {flowTargetLabel(view.replaces)} is waiting for its code. Starting one here
            replaces it, and its code stops working.
          </p>
          <div className="flex gap-2">
            <Button size="sm" onClick={begin}>
              Start anyway
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setConfirming(false)}>
              Keep it
            </Button>
          </div>
        </div>
      )}
      {view.shown && (
        <div className="space-y-1">
          {view.shown.target && (
            <p className="text-xs text-muted-foreground">Sign-in {flowTargetLabel(view.shown.target)}</p>
          )}
          <DeviceFlowPanel flow={view.shown} />
        </div>
      )}
      {start.error && <p className="text-sm text-destructive">{apiErrorMessage(start.error)}</p>}
      <Collapsible open={pasteOpen} onOpenChange={(open) => setPaste({ open, flowId: shownId })}>
        <CollapsibleTrigger>Paste auth.json instead</CollapsibleTrigger>
        <CollapsibleContent className="pt-2">
          <PasteLogin kind="chatgpt" target={target} />
        </CollapsibleContent>
      </Collapsible>
    </div>
  );
}

/** Remove the login `target` holds, once the person confirms. */
export function DisconnectButton({ kind, target }: { kind: SubscriptionKind; target: LoginTarget }) {
  const remove = useRemoveLogin();
  return (
    <div className="space-y-1">
      <Button
        size="sm"
        variant="outline"
        onClick={() => {
          if (confirm(`Disconnect this ${KIND_LABELS[kind]} login?`)) remove.mutate({ kind, target });
        }}
        disabled={remove.isPending}
      >
        <Trash2 className="h-4 w-4 mr-1" />
        Disconnect
      </Button>
      {remove.error && <p className="text-sm text-destructive">{apiErrorMessage(remove.error)}</p>}
    </div>
  );
}

const deviceTarget = (target: LoginTarget): DeviceTarget | null =>
  target.tier === "system_shared" ? null : { tier: target.tier, agent_id: target.agent_id };

/** Connect a login to one tier — a pasted Claude token, a ChatGPT device
 *  sign-in or pasted `auth.json` — and disconnect the one it holds. */
export function LoginConnect({
  kind,
  target,
  connected,
}: {
  kind: SubscriptionKind;
  target: LoginTarget;
  connected: boolean;
}) {
  const device = kind === "chatgpt" ? deviceTarget(target) : null;
  return (
    <div className="space-y-3">
      {device ? <DeviceConnect target={device} /> : <PasteLogin kind={kind} target={target} />}
      {connected && <DisconnectButton kind={kind} target={target} />}
    </div>
  );
}
