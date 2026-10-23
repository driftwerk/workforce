import { useState } from "react";
import { Link } from "react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowRightLeft, Loader2, RefreshCw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { label, OperationOutcome, RecipientSelect } from "@/components/identity-lifecycle";
import { apiErrorMessage } from "@/lib/api-error";
import {
  identitiesApi,
  isPreview,
  type HeldCounts,
  type Orphan,
  type UnclaimedName,
} from "@/lib/api/identities";

const SCAN_KEY = ["identity-lifecycle", "orphans"];

const SELECT = "rounded-md border px-3 py-2 text-sm bg-background";

function counts(held: HeldCounts): string {
  return Object.entries(held)
    .map(([category, n]) => `${label(category)} ${n}`)
    .join(", ");
}

type Action = "delete" | "transfer";

const WHAT_HAPPENS: Record<Action, string> = {
  transfer:
    "Jobs, workflows, playbooks, monitors, shared artifacts and skills go to the recipient as your edit, so they run without the recipient's private context until they save them. Credentials, devices, roles, team memberships, permissions and private artifacts are erased, and nothing it went by is linked to the recipient. Billing and audit records are kept.",
  delete:
    "Erases what it left. Billing and audit records are kept. With a recipient, shared artifacts and skills others rely on go to them.",
};

function OrphanCard({ orphan }: { orphan: Orphan }) {
  const qc = useQueryClient();
  const [action, setAction] = useState<Action>("delete");
  const [to, setTo] = useState("");
  const [movePersonal, setMovePersonal] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const run = useMutation({
    mutationFn: (dryRun: boolean) =>
      action === "transfer"
        ? identitiesApi.transferOrphan(orphan.id, { to, move_personal: movePersonal, dry_run: dryRun })
        : identitiesApi.deleteOrphan(orphan.id, { recipient: to || undefined, dry_run: dryRun }),
  });
  const finished = () => qc.invalidateQueries({ queryKey: SCAN_KEY });
  const reset = () => {
    run.reset();
    setConfirm(false);
  };
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex flex-wrap items-center gap-2 text-base">
          <span className="break-all font-mono">{orphan.id}</span>
          <Badge variant="outline">
            {orphan.proof === "recorded" ? "recorded by a lifecycle operation" : "held in an identity field"}
          </Badge>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        {orphan.names && orphan.names.length > 0 && (
          <p className="break-all text-muted-foreground">Also went by {orphan.names.join(", ")}</p>
        )}
        <p>{counts(orphan.held)}</p>
        <div className="flex flex-wrap items-center gap-2">
          <select
            aria-label={`What to do with what ${orphan.id} left`}
            className={SELECT}
            value={action}
            onChange={(e) => {
              setAction(e.target.value as Action);
              setMovePersonal(false);
              reset();
            }}
          >
            <option value="delete">Erase it</option>
            <option value="transfer">Hand it to someone</option>
          </select>
          <RecipientSelect
            subject={orphan.id}
            value={to}
            onChange={(id) => {
              setTo(id);
              reset();
            }}
            placeholder={action === "transfer" ? "Hand to…" : "No recipient"}
            ariaLabel={`Recipient of what ${orphan.id} left`}
          />
          <Button
            variant="outline"
            disabled={(action === "transfer" && !to) || run.isPending}
            onClick={() => {
              setConfirm(false);
              run.mutate(true);
            }}
          >
            Preview
          </Button>
          <Button
            variant={action === "delete" ? "destructive" : "default"}
            disabled={!isPreview(run.data) || run.isPending}
            onClick={() => (confirm ? run.mutate(false) : setConfirm(true))}
          >
            {run.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : action === "delete" ? (
              <Trash2 className="mr-1.5 h-4 w-4" />
            ) : (
              <ArrowRightLeft className="mr-1.5 h-4 w-4" />
            )}
            {action === "delete" ? (confirm ? "Confirm erase" : "Erase") : confirm ? "Confirm hand-over" : "Hand over"}
          </Button>
        </div>
        <p className="text-muted-foreground">{WHAT_HAPPENS[action]}</p>
        {action === "transfer" && (
          <label className="flex items-start gap-2">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={movePersonal}
              onChange={(e) => {
                setMovePersonal(e.target.checked);
                reset();
              }}
            />
            <span>
              Also move its conversations, memories, notes, documents and usage and cost history
              to the recipient. Only do this if you know the recipient is the same person:
              otherwise they stay where they are, and you can erase what is personal afterwards.
            </span>
          </label>
        )}
        {run.error && (
          <p className="text-destructive" role="alert">
            {apiErrorMessage(run.error)}
          </p>
        )}
        {run.data && <OperationOutcome outcome={run.data} onFinished={finished} />}
      </CardContent>
    </Card>
  );
}

function Unclaimed({ title, note, names }: { title: string; note: string; names: UnclaimedName[] }) {
  if (names.length === 0) return null;
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{title}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-2 text-sm">
        <p className="text-muted-foreground">{note}</p>
        <ul className="space-y-1">
          {names.map((n) => (
            <li key={n.name} className="break-all">
              <span className="font-mono">{n.name}</span>: {counts(n.held)}
              {n.could_be && n.could_be.length > 0 && (
                <span className="text-muted-foreground"> (could be {n.could_be.join(", ")})</span>
              )}
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

/** Data left by identities that no longer exist, and handing it over or erasing it. */
export default function OrphansPage() {
  const scan = useQuery({ queryKey: SCAN_KEY, queryFn: identitiesApi.orphans });
  return (
    <div className="space-y-6 p-6">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h1 className="text-2xl font-bold">Orphaned data</h1>
          <p className="text-sm text-muted-foreground">
            What identities that no longer exist left behind. <Link to="/users" className="text-primary hover:underline">Back to users</Link>
          </p>
        </div>
        <Button variant="outline" disabled={scan.isFetching} onClick={() => scan.refetch()}>
          <RefreshCw className="mr-1.5 h-4 w-4" />
          Scan again
        </Button>
      </div>
      {scan.isLoading && <Loader2 className="h-5 w-5 animate-spin" />}
      {scan.error && (
        <p className="text-sm text-destructive" role="alert">
          {apiErrorMessage(scan.error)}
        </p>
      )}
      {scan.data && (
        <>
          {!scan.data.complete && (
            <div className="space-y-1 text-sm text-amber-700 dark:text-amber-400" role="status">
              <p>Not everything could be read, so the lists below may be short.</p>
              {Object.entries(scan.data.failed ?? {}).map(([category, why]) => (
                <p key={category}>
                  {label(category)}: {why}
                </p>
              ))}
            </div>
          )}
          {scan.data.orphans.length === 0 && (
            <p className="text-sm text-muted-foreground">Nothing is held for a missing identity.</p>
          )}
          {scan.data.orphans.map((orphan) => (
            <OrphanCard key={orphan.id} orphan={orphan} />
          ))}
          <Unclaimed
            title="Unproven ids"
            note="Identity-shaped ids no identity has and nothing proves were ever identities. They are left alone."
            names={scan.data.unproven}
          />
          <Unclaimed
            title="Shared by several missing identities"
            note="Names more than one missing identity went by. They are left alone."
            names={scan.data.ambiguous}
          />
          {Object.keys(scan.data.not_orphaned).length > 0 && (
            <p className="text-sm text-muted-foreground">
              Also holding things, and never touched here:{" "}
              {Object.entries(scan.data.not_orphaned)
                .map(([kind, n]) => `${n} ${kind.replace(/_/g, " ")}`)
                .join(", ")}
              .
            </p>
          )}
        </>
      )}
    </div>
  );
}
