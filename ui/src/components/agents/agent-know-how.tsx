import { useState } from "react";
import { Ban, Lightbulb, Pin, Trash2 } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { EmptyState } from "@/components/empty-state";
import {
  useAgentKnowHow,
  useDeleteKnowHow,
  usePinKnowHow,
  useRetireKnowHow,
} from "@/hooks/use-agent-know-how";
import type { KnowHowEntry, KnowHowKind, KnowHowStatus } from "@/hooks/use-agent-know-how";
import { ApiError } from "@/lib/api-client";
import { apiErrorMessage } from "@/lib/api-error";
import { formatRelativeTime } from "@/lib/utils";

const KIND_LABELS: Record<KnowHowKind, string> = {
  data_location: "Data location",
  query_shape: "Query shape",
  schema_correction: "Correction",
  code_location: "Code location",
};

const STATUS_LABELS: Record<KnowHowStatus, string> = {
  pinned: "Pinned",
  shared: "Shared",
  pending: "Awaiting review",
  private: "Not yet shared",
  stale: "Stale",
};

const STATUS_VARIANTS: Record<
  KnowHowStatus,
  "default" | "secondary" | "outline" | "success" | "warning"
> = {
  pinned: "default",
  shared: "success",
  pending: "warning",
  private: "secondary",
  stale: "outline",
};

type Action = "retire" | "delete";

const ACTION_TEXT: Record<Action, { title: string; description: string; confirm: string }> = {
  retire: {
    title: "Retire entry",
    description:
      "Nobody sees it again, and later turns never learn it again. Use this for know-how that is wrong.",
    confirm: "Retire",
  },
  delete: {
    title: "Delete entry",
    description:
      "Removes it and everything recorded for it. Later turns may learn it afresh if people keep using these names.",
    confirm: "Delete",
  },
};

interface AgentKnowHowProps {
  agentId: string;
}

export function AgentKnowHow({ agentId }: AgentKnowHowProps) {
  const { data, isLoading, error } = useAgentKnowHow(agentId);
  const pin = usePinKnowHow(agentId);
  const retire = useRetireKnowHow(agentId);
  const remove = useDeleteKnowHow(agentId);
  const [target, setTarget] = useState<{ entry: KnowHowEntry; action: Action } | null>(null);

  if (isLoading) {
    return <div className="py-12 text-center text-sm text-muted-foreground">Loading…</div>;
  }
  if (error) {
    return <KnowHowError error={error} />;
  }
  const entries = data?.entries ?? [];
  const minUsers = data?.min_users ?? 2;
  const confirming = target ? (target.action === "retire" ? retire : remove) : null;
  const closeConfirm = () => {
    confirming?.reset();
    setTarget(null);
  };

  return (
    <div className="space-y-4">
      <div>
        <h3 className="text-lg font-semibold">Know-how</h3>
        <p className="text-sm text-muted-foreground">
          Where this agent has found data and code for its users, learned from the tool calls
          that answered them. Only checked names are kept, never what anyone asked; a name is
          shared once {minUsers} different people used it, and until then it is hidden here too.
        </p>
      </div>

      {pin.error && (
        <div className="rounded-md border border-destructive/30 bg-destructive/10 p-3 text-sm text-destructive">
          {apiErrorMessage(pin.error)}
        </div>
      )}

      {entries.length === 0 ? (
        <EmptyState
          icon={Lightbulb}
          title="Nothing learned yet"
          description="Entries appear here as people's questions are answered from data and code."
        />
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-36">
                  Kind
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                  Names
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-36">
                  Status
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-20">
                  People
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-20">
                  Shown
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-32">
                  Last used
                </th>
                <th className="w-32 px-4 py-3" />
              </tr>
            </thead>
            <tbody>
              {entries.map((entry) => (
                <tr key={entry.id} className="border-b last:border-0 hover:bg-muted/30">
                  <td className="px-4 py-3 text-sm">{KIND_LABELS[entry.kind]}</td>
                  <td className="px-4 py-3">
                    <EntryNames entry={entry} minUsers={minUsers} />
                  </td>
                  <td className="px-4 py-3">
                    <Badge variant={STATUS_VARIANTS[entry.status]}>
                      {STATUS_LABELS[entry.status]}
                    </Badge>
                  </td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">{entry.people}</td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">{entry.uses}</td>
                  <td className="px-4 py-3 text-xs text-muted-foreground">
                    {entry.last_seen ? formatRelativeTime(entry.last_seen) : "—"}
                  </td>
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-1">
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Pin: show to everyone who can reach it"
                        disabled={
                          entry.status === "pinned" ||
                          entry.status === "private" ||
                          entry.status === "stale" ||
                          pin.isPending
                        }
                        onClick={() => pin.mutate(entry.id)}
                      >
                        <Pin className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Retire"
                        onClick={() => setTarget({ entry, action: "retire" })}
                      >
                        <Ban className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Delete"
                        onClick={() => setTarget({ entry, action: "delete" })}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <Dialog open={target !== null} onOpenChange={(open) => !open && closeConfirm()}>
        <DialogContent>
          {target && confirming && (
            <>
              <DialogHeader>
                <DialogTitle>{ACTION_TEXT[target.action].title}</DialogTitle>
                <DialogDescription>{ACTION_TEXT[target.action].description}</DialogDescription>
              </DialogHeader>
              <div className="rounded-md border p-3">
                <EntryNames entry={target.entry} minUsers={minUsers} />
              </div>
              {confirming.error && (
                <p className="text-sm text-destructive" role="alert">
                  {apiErrorMessage(confirming.error)}
                </p>
              )}
              <DialogFooter>
                <Button variant="outline" onClick={closeConfirm}>
                  Cancel
                </Button>
                <Button
                  variant="destructive"
                  disabled={confirming.isPending}
                  onClick={() =>
                    confirming.mutate(target.entry.id, { onSuccess: closeConfirm })
                  }
                >
                  {ACTION_TEXT[target.action].confirm}
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}

function EntryNames({ entry, minUsers }: { entry: KnowHowEntry; minUsers: number }) {
  if (entry.names_hidden) {
    return (
      <div className="text-xs italic text-muted-foreground">
        Names hidden until {minUsers} people use them
      </div>
    );
  }
  const where =
    entry.kind === "code_location"
      ? entry.repo
      : entry.database_id !== undefined
        ? `database ${entry.database_id}`
        : null;
  return (
    <div className="space-y-0.5">
      <div className="font-mono text-xs break-all">
        {entry.kind === "code_location" && (
          <>
            {entry.path}
            {entry.symbol && <span className="text-muted-foreground"> · {entry.symbol}</span>}
          </>
        )}
        {entry.kind === "schema_correction" && (
          <>
            <span className="line-through text-muted-foreground">{entry.wrong}</span>
            {" → "}
            {entry.columns[0] ?? entry.tables[0]}
          </>
        )}
        {entry.kind === "query_shape" && entry.shape}
        {entry.kind === "data_location" && (
          <>
            {entry.tables.join(", ")}
            {entry.columns.length > 0 && (
              <span className="text-muted-foreground"> ({entry.columns.join(", ")})</span>
            )}
          </>
        )}
      </div>
      <div className="text-xs text-muted-foreground">
        {where}
        {entry.note && ` · ${entry.note}`}
        {entry.manual && " · added by hand"}
      </div>
    </div>
  );
}

function KnowHowError({ error }: { error: unknown }) {
  if (error instanceof ApiError && error.status === 503) {
    return (
      <div className="rounded-md border border-warning/30 bg-warning/10 p-4 text-sm">
        Know-how is off on this hub. Set <span className="font-mono">[know_how] mode</span> in{" "}
        <span className="font-mono">workforce.toml</span>, then turn it on for this agent in its{" "}
        <span className="font-mono">config.toml</span>.
      </div>
    );
  }
  return (
    <div className="rounded-md border border-destructive/30 bg-destructive/10 p-4 text-sm text-destructive">
      Failed to load know-how: {apiErrorMessage(error)}
    </div>
  );
}
