import { useState, type FormEvent } from "react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { InlineCode } from "@/components/ui/inline-code";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  useAgentSlack,
  useConnectAgentSlack,
  useDisconnectAgentSlack,
} from "@/hooks/use-agents";
import type { SlackWorkspaceStatus } from "@/lib/api/agents";
import { apiErrorMessage } from "@/lib/api-error";

function WorkspaceRow({ workspace }: { workspace: SlackWorkspaceStatus }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-3">
      <div className="space-y-1">
        <p className="text-sm font-medium">
          Workspace <InlineCode>{workspace.workspace_id}</InlineCode>
        </p>
        <p className="text-xs text-muted-foreground">
          {workspace.connected
            ? `Bot ${workspace.bot_name ?? ""} (${workspace.bot_user_id ?? ""})`
            : (workspace.reason ?? "Not connected")}
        </p>
      </div>
      <Badge variant={workspace.connected ? "success" : "warning"}>
        {workspace.connected ? "Connected" : "Not connected"}
      </Badge>
    </div>
  );
}

/** Connect an agent to its own Slack app, rotate its tokens, or disconnect it. */
export function AgentSlack({ agentId }: { agentId: string }) {
  const { data, isLoading, error } = useAgentSlack(agentId);
  const connect = useConnectAgentSlack(agentId);
  const disconnect = useDisconnectAgentSlack(agentId);
  const [botToken, setBotToken] = useState("");
  const [appToken, setAppToken] = useState("");
  const [signingSecret, setSigningSecret] = useState("");
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);

  const workspaces = data?.workspaces ?? [];
  const bound = workspaces.length > 0;

  function submit(event: FormEvent) {
    event.preventDefault();
    connect.mutate(
      {
        bot_token: botToken.trim(),
        app_token: appToken.trim(),
        ...(signingSecret.trim() ? { signing_secret: signingSecret.trim() } : {}),
      },
      {
        onSuccess: () => {
          setBotToken("");
          setAppToken("");
          setSigningSecret("");
        },
      },
    );
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Slack</CardTitle>
        <CardDescription>
          Each agent answers in Slack as its own Slack app. Create one from the manifest the
          agent's <InlineCode>slack.app_scaffold</InlineCode> tool generates, install it, then
          paste its tokens here. The tokens are checked with Slack, stored in this agent's vault,
          and the connection starts on every hub replica without a restart.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading ? (
          <Skeleton className="h-16 w-full" />
        ) : error ? (
          <p className="text-sm text-destructive">{apiErrorMessage(error)}</p>
        ) : bound ? (
          <div className="space-y-2">
            {workspaces.map((workspace) => (
              <WorkspaceRow key={workspace.workspace_id} workspace={workspace} />
            ))}
          </div>
        ) : (
          <p className="text-sm text-muted-foreground">This agent is not connected to Slack.</p>
        )}

        <form onSubmit={submit} className="space-y-3">
          <Input
            type="password"
            autoComplete="off"
            placeholder="Bot User OAuth Token (xoxb-…)"
            value={botToken}
            onChange={(e) => setBotToken(e.target.value)}
          />
          <Input
            type="password"
            autoComplete="off"
            placeholder="App-level token with connections:write (xapp-…)"
            value={appToken}
            onChange={(e) => setAppToken(e.target.value)}
          />
          <Input
            type="password"
            autoComplete="off"
            placeholder="Signing secret (optional)"
            value={signingSecret}
            onChange={(e) => setSigningSecret(e.target.value)}
          />
          {connect.error && (
            <p className="text-sm text-destructive">{apiErrorMessage(connect.error)}</p>
          )}
          {connect.data && (
            <p className="text-sm text-muted-foreground">
              Connected as {connect.data.bot_name} in workspace{" "}
              <InlineCode>{connect.data.workspace_id}</InlineCode>.
            </p>
          )}
          <div className="flex flex-wrap gap-2">
            <Button
              type="submit"
              disabled={connect.isPending || !botToken.trim() || !appToken.trim()}
            >
              {connect.isPending ? "Connecting…" : bound ? "Replace tokens" : "Connect"}
            </Button>
            {bound && (
              <Button
                type="button"
                variant="outline"
                onClick={() => setConfirmDisconnect(true)}
              >
                Disconnect
              </Button>
            )}
          </div>
        </form>
      </CardContent>

      <Dialog open={confirmDisconnect} onOpenChange={setConfirmDisconnect}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Disconnect from Slack?</DialogTitle>
            <DialogDescription>
              The agent's Slack tokens are deleted from the vault and its workspace is removed
              from the Slack config. Its bot stops answering on every hub replica.
            </DialogDescription>
          </DialogHeader>
          {disconnect.error && (
            <p className="text-sm text-destructive">{apiErrorMessage(disconnect.error)}</p>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmDisconnect(false)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={disconnect.isPending}
              onClick={() =>
                disconnect.mutate(undefined, {
                  onSuccess: () => {
                    connect.reset();
                    setConfirmDisconnect(false);
                  },
                })
              }
            >
              {disconnect.isPending ? "Disconnecting…" : "Disconnect"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </Card>
  );
}
