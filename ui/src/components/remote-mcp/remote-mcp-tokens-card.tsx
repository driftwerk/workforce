import { useState } from "react";
import { Check, Copy, KeyRound, Loader2, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { InlineCode } from "@/components/ui/inline-code";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { TableSkeleton } from "@/components/loading-skeleton";
import {
  useCreateRemoteMcpToken,
  useRemoteMcpTokens,
  useRevokeRemoteMcpToken,
} from "@/hooks/use-my-settings";
import type { CreatedRemoteMcpToken, RemoteMcpToken } from "@/lib/api/my-settings";
import { apiErrorMessage } from "@/lib/api-error";
import { formatRelativeTime } from "@/lib/utils";

const MAX_LABEL_LENGTH = 64;

type CopyState = "idle" | "copied" | "failed";

function CopyButton({ text, label }: { text: string; label: string }) {
  const [state, setState] = useState<CopyState>("idle");
  const handleCopy = () => {
    const write = navigator.clipboard?.writeText(text) ?? Promise.reject(new Error("unavailable"));
    write
      .then(() => setState("copied"))
      .catch(() => setState("failed"))
      .finally(() => setTimeout(() => setState("idle"), 2000));
  };
  return (
    <Button size="sm" variant="outline" onClick={handleCopy} aria-label={label}>
      {state === "copied" ? (
        <Check className="h-3.5 w-3.5 mr-1" />
      ) : (
        <Copy className="h-3.5 w-3.5 mr-1" />
      )}
      {state === "copied" ? "Copied" : state === "failed" ? "Select and copy" : "Copy"}
    </Button>
  );
}

function CopyableBlock({ title, text }: { title: string; text: string }) {
  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between">
        <span className="text-sm font-medium">{title}</span>
        <CopyButton text={text} label={`Copy ${title.toLowerCase()}`} />
      </div>
      <pre className="rounded-md bg-muted p-3 text-xs whitespace-pre-wrap break-all">{text}</pre>
    </div>
  );
}

function CreatedTokenDialog({
  created,
  onClose,
}: {
  created: CreatedRemoteMcpToken | null;
  onClose: () => void;
}) {
  return (
    <Dialog open={created !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>Token created</DialogTitle>
          <DialogDescription>
            This token is shown once. Copy it now — it cannot be retrieved later, only revoked.
          </DialogDescription>
        </DialogHeader>
        {created && (
          <div className="space-y-4">
            <CopyableBlock title={`Token (${created.label})`} text={created.token} />
            <CopyableBlock title="Claude Code" text={created.connect.claude_code} />
            <CopyableBlock title="workforce-local-mcp" text={created.connect.workforce_local} />
            <p className="text-xs text-muted-foreground">
              Expires {new Date(created.expires_at).toLocaleDateString()}.
            </p>
          </div>
        )}
        <DialogFooter>
          <Button onClick={onClose}>Done</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function TokenRow({
  token,
  onRevoke,
  revoking,
}: {
  token: RemoteMcpToken;
  onRevoke: () => void;
  revoking: boolean;
}) {
  return (
    <tr className={`border-b last:border-b-0 ${token.expired ? "opacity-60" : ""}`}>
      <td className="px-4 py-3">
        <div className="flex items-center gap-2">
          <span className="font-medium">{token.label}</span>
          {token.expired && <Badge variant="outline">Expired</Badge>}
        </div>
        <code className="text-xs text-muted-foreground">wfr_{token.token_prefix}…</code>
      </td>
      <td className="px-4 py-3 text-sm text-muted-foreground">
        {new Date(token.created_at).toLocaleDateString()}
      </td>
      <td className="px-4 py-3 text-sm text-muted-foreground">
        {new Date(token.expires_at).toLocaleDateString()}
      </td>
      <td className="px-4 py-3 text-sm text-muted-foreground">
        {token.last_used_at ? formatRelativeTime(token.last_used_at) : "never"}
      </td>
      <td className="px-4 py-3 text-right">
        <Button size="sm" variant="outline" onClick={onRevoke} disabled={revoking}>
          <Trash2 className="h-4 w-4 mr-1" />
          Revoke
        </Button>
      </td>
    </tr>
  );
}

function TokenTable({ tokens }: { tokens: RemoteMcpToken[] }) {
  const revoke = useRevokeRemoteMcpToken();
  const [error, setError] = useState<string | null>(null);

  function handleRevoke(token: RemoteMcpToken) {
    if (!confirm(`Revoke the token "${token.label}"? Clients using it will stop working immediately.`)) {
      return;
    }
    setError(null);
    revoke.mutate(token.id, { onError: (e) => setError(apiErrorMessage(e)) });
  }

  if (tokens.length === 0) {
    return <p className="text-sm text-muted-foreground">No tokens yet.</p>;
  }

  return (
    <div className="space-y-2">
      {error && <p className="text-sm text-destructive">{error}</p>}
      <div className="overflow-x-auto rounded-md border">
        <table className="w-full text-sm">
          <thead className="bg-muted/50 text-left text-xs uppercase text-muted-foreground">
            <tr>
              <th className="px-4 py-2 font-medium">Token</th>
              <th className="px-4 py-2 font-medium">Created</th>
              <th className="px-4 py-2 font-medium">Expires</th>
              <th className="px-4 py-2 font-medium">Last used</th>
              <th className="px-4 py-2 font-medium text-right">Actions</th>
            </tr>
          </thead>
          <tbody>
            {tokens.map((token) => (
              <TokenRow
                key={token.id}
                token={token}
                revoking={revoke.isPending && revoke.variables === token.id}
                onRevoke={() => handleRevoke(token)}
              />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function CreateTokenForm({ onCreated }: { onCreated: (created: CreatedRemoteMcpToken) => void }) {
  const [label, setLabel] = useState("");
  const [error, setError] = useState<string | null>(null);
  const create = useCreateRemoteMcpToken();
  const trimmed = label.trim();

  function handleCreate() {
    setError(null);
    create.mutate(trimmed, {
      onSuccess: (created) => {
        setLabel("");
        onCreated(created);
      },
      onError: (e) => setError(apiErrorMessage(e)),
    });
  }

  return (
    <div className="space-y-2">
      <div className="flex gap-2">
        <Input
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && trimmed && !create.isPending) handleCreate();
          }}
          placeholder="Label, e.g. laptop"
          maxLength={MAX_LABEL_LENGTH}
          className="max-w-xs"
        />
        <Button size="sm" onClick={handleCreate} disabled={!trimmed || create.isPending}>
          {create.isPending ? (
            <Loader2 className="h-4 w-4 animate-spin mr-1" />
          ) : (
            <Plus className="h-4 w-4 mr-1" />
          )}
          Create token
        </Button>
      </div>
      {error && <p className="text-sm text-destructive">{error}</p>}
    </div>
  );
}

export function RemoteMcpTokensCard() {
  const { data, isLoading, error } = useRemoteMcpTokens();
  const [created, setCreated] = useState<CreatedRemoteMcpToken | null>(null);

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-3">
          <div className="rounded-lg bg-muted p-2">
            <KeyRound className="h-5 w-5 text-muted-foreground" />
          </div>
          <div>
            <CardTitle className="text-base">Remote MCP tokens</CardTitle>
            <CardDescription>
              Connect Claude Code or workforce-local-mcp to this install as you. Tokens carry
              your own permissions, last 90 days, and can be revoked here at any time.
            </CardDescription>
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {isLoading ? (
          <TableSkeleton rows={2} />
        ) : error ? (
          <p className="text-sm text-destructive">
            Failed to load tokens: {apiErrorMessage(error)}
          </p>
        ) : !data || !data.enabled ? (
          <p className="text-sm text-muted-foreground">
            Remote MCP is not enabled on this install. An admin can turn it on with{" "}
            <InlineCode>[remote_mcp] enabled = true</InlineCode>.
          </p>
        ) : (
          <>
            <CreateTokenForm onCreated={setCreated} />
            <TokenTable tokens={data.tokens} />
          </>
        )}
        <CreatedTokenDialog created={created} onClose={() => setCreated(null)} />
      </CardContent>
    </Card>
  );
}
