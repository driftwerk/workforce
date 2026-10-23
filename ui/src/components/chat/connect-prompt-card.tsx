import { Link } from "react-router";
import { Loader2 } from "lucide-react";
import { DeviceFlowPanel } from "@/components/subscriptions/device-flow-panel";
import { PASTE_HINTS } from "@/components/subscriptions/login-connect";
import { useDeviceFlow } from "@/hooks/use-subscriptions";
import type { ConnectPrompt } from "@/lib/api/subscriptions";
import { apiErrorMessage } from "@/lib/api-error";
import { afterConnect, nextFlowUi } from "@/lib/subscriptions/device-flow";
import { KIND_LABELS } from "@/lib/subscriptions/status";

/** Offers the person a turn was for a way to connect the plan it needed. The
 *  sign-in code shows only in their own private session, read through the
 *  owner-checked poll; everywhere else, and once the sign-in has ended, the
 *  card points to My Settings. */
export function ConnectPromptCard({ prompt }: { prompt: ConnectPrompt }) {
  const flow = useDeviceFlow(prompt.private ? prompt.flow_id : null);
  const ui = nextFlowUi(flow.data, false);
  const kind = KIND_LABELS[prompt.kind];
  const settings = (
    <Link to={`/my-settings#${prompt.kind}`} className="underline">
      My Settings
    </Link>
  );

  return (
    <div className="space-y-2 rounded-lg border px-4 py-3">
      <p className="text-sm font-medium">Connect your {kind} plan</p>
      <p className="text-sm text-muted-foreground">{prompt.reason}</p>
      {flow.data && <DeviceFlowPanel flow={flow.data} then={afterConnect(prompt)} />}
      {flow.error && <p className="text-sm text-destructive">{apiErrorMessage(flow.error)}</p>}
      {flow.isLoading && (
        <p className="flex items-center gap-1 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          Loading your sign-in code…
        </p>
      )}
      {!flow.isLoading && !ui.pending && !ui.connected && (
        <p className="text-sm">
          {prompt.kind === "claude" ? (
            <>
              {PASTE_HINTS.claude.hint} Paste it in {settings}.
            </>
          ) : (
            <>Sign in from {settings}.</>
          )}
        </p>
      )}
    </div>
  );
}
