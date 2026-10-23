import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2, Merge, Trash2, ArrowRightLeft } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useIdentities } from "@/hooks/use-identities";
import { apiErrorMessage } from "@/lib/api-error";
import {
  identitiesApi,
  isPreview,
  isStarted,
  type CategoryReport,
  type DeleteRule,
  type LifecycleChoice,
  type LifecycleOperation,
  type LifecycleOutcome,
  type LifecycleReport,
  type TransferItem,
} from "@/lib/api/identities";

const SELECT = "rounded-md border px-3 py-2 text-sm bg-background";

export function label(category: string): string {
  return category.replace(/_/g, " ");
}

function deleteRule(rule: DeleteRule): string {
  return rule.rule === "choice" ? `${rule.default} (choice)` : rule.rule;
}

/** Everyone the subject's things could go to: every other identity. */
function useRecipients(subject: string) {
  const { data } = useIdentities();
  return (data?.identities ?? []).filter((i) => i.id !== subject);
}

export function RecipientSelect({
  subject,
  value,
  onChange,
  placeholder,
  ariaLabel,
}: {
  subject: string;
  value: string;
  onChange: (id: string) => void;
  placeholder: string;
  ariaLabel: string;
}) {
  const recipients = useRecipients(subject);
  return (
    <select
      aria-label={ariaLabel}
      className={SELECT}
      value={value}
      onChange={(e) => onChange(e.target.value)}
    >
      <option value="">{placeholder}</option>
      {recipients.map((r) => (
        <option key={r.id} value={r.id}>
          {r.display_name}
          {r.email ? ` (${r.email})` : ""}
        </option>
      ))}
    </select>
  );
}

/** What a dry run or a real run did, category by category. */
export function ReportSummary({ report }: { report: LifecycleReport }) {
  const touched = report.categories.filter(
    (c) => c.count > 0 || c.error || c.conflicts?.length || c.blocking?.length,
  );
  return (
    <div className="space-y-2 text-sm" role="status">
      <p className="font-medium">
        {report.dry_run
          ? "Preview: nothing has changed yet."
          : report.complete
            ? "Done."
            : "Not finished: resolve what is listed below and run it again."}
      </p>
      {report.error && <p className="text-destructive">{report.error}</p>}
      {touched.length === 0 && <p className="text-muted-foreground">Nothing is held.</p>}
      <ul className="space-y-1">
        {touched.map((c) => (
          <li key={c.category}>
            <span className="font-medium">{label(c.category)}</span>: {c.count}
            {c.orphan ? ` (${label(c.orphan)})` : c.choice ? ` (${c.choice})` : ""}
            {c.error && <span className="text-destructive"> — failed: {c.error}</span>}
            {c.conflicts?.map((note) => (
              <div key={note} className="pl-4 text-muted-foreground">
                {note}
              </div>
            ))}
            {c.blocking?.map((note) => (
              <div key={note} className="pl-4 text-amber-700 dark:text-amber-400">
                {note}
              </div>
            ))}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** How often a running operation's record is read again. */
const POLL_MS = 2000;

function running(operation: LifecycleOperation | undefined): boolean {
  return operation?.status === "running";
}

/** A real run as it goes: its progress, read from its record, then its report. */
function RunningOperation({
  started,
  onFinished,
}: {
  started: LifecycleOperation;
  onFinished?: (report: LifecycleReport) => void;
}) {
  const record = useQuery({
    queryKey: ["identity-lifecycle", "operation", started.id],
    queryFn: () => identitiesApi.operation(started.id),
    initialData: { operation: started },
    refetchInterval: (query) => (running(query.state.data?.operation) ? POLL_MS : false),
  });
  const operation = record.data.operation;
  const finished = useRef(false);
  useEffect(() => {
    if (!running(operation) && !finished.current) {
      finished.current = true;
      onFinished?.(operation.report as LifecycleReport);
    }
  }, [operation, onFinished]);

  if (running(operation)) {
    const progress = operation.report.progress;
    const stalled = new Date(operation.lease_until).getTime() < Date.now();
    return (
      <div className="space-y-1 text-sm" role="status">
        <p className="flex items-center gap-2 font-medium">
          <Loader2 className="h-4 w-4 animate-spin" />
          {progress
            ? `Running: ${progress.done} of ${progress.total} done, now ${label(progress.category)}.`
            : "Starting…"}
        </p>
        <p className="text-muted-foreground">
          {stalled
            ? "Its hub stopped answering. Run it again to resume it."
            : "It carries on if you leave this page."}
        </p>
        {record.error && <p className="text-destructive">{apiErrorMessage(record.error)}</p>}
      </div>
    );
  }
  return <ReportSummary report={operation.report as LifecycleReport} />;
}

/** A dry run's report, or the real run it started as that run goes. */
export function OperationOutcome({
  outcome,
  onFinished,
}: {
  outcome: LifecycleOutcome;
  onFinished?: (report: LifecycleReport) => void;
}) {
  return isStarted(outcome) ? (
    <RunningOperation key={outcome.operation.id} started={outcome.operation} onFinished={onFinished} />
  ) : (
    <ReportSummary report={outcome} />
  );
}

function HoldingsTable({ categories }: { categories: CategoryReport[] }) {
  return (
    <table className="w-full text-sm">
      <thead>
        <tr className="border-b text-left text-muted-foreground">
          <th className="px-2 py-2 font-medium">What</th>
          <th className="px-2 py-2 font-medium">Held</th>
          <th className="px-2 py-2 font-medium">On merge</th>
          <th className="px-2 py-2 font-medium">On delete</th>
        </tr>
      </thead>
      <tbody>
        {categories.map((c) => (
          <tr key={c.category} className="border-b align-top">
            <td className="px-2 py-2">
              <div className="font-medium">{label(c.category)}</div>
              {c.items && c.items.length > 0 && (
                <div className="text-xs text-muted-foreground break-all">
                  {c.items.slice(0, 5).join(", ")}
                  {c.count > 5 ? ` and ${c.count - 5} more` : ""}
                </div>
              )}
              {c.error && <div className="text-xs text-destructive">{c.error}</div>}
            </td>
            <td className="px-2 py-2">{c.count}</td>
            <td className="px-2 py-2">
              <Badge variant="outline">{c.rules.merge}</Badge>
            </td>
            <td className="px-2 py-2">
              <Badge variant="outline">{deleteRule(c.rules.delete)}</Badge>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function MergeCard({ subject }: { subject: string }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [into, setInto] = useState("");
  const [confirm, setConfirm] = useState(false);
  const run = useMutation({
    mutationFn: (dryRun: boolean) => identitiesApi.merge(subject, into, dryRun),
  });
  const finished = (report: LifecycleReport) => {
    qc.invalidateQueries({ queryKey: ["identities"] });
    if (report.complete) navigate(`/users/${into}`);
  };
  const previewed = isPreview(run.data) && run.data.target === into;
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Merge into another identity</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          Everything this identity owns, its usage and cost history included, moves to the one
          you choose, its jobs and workflows keep running as that identity, and this identity is
          retired. Roles and other authority are not carried over.
        </p>
        <div className="flex flex-wrap gap-2">
          <RecipientSelect
            subject={subject}
            value={into}
            onChange={(id) => {
              setInto(id);
              run.reset();
              setConfirm(false);
            }}
            placeholder="Merge into…"
            ariaLabel="Identity to merge into"
          />
          <Button
            variant="outline"
            disabled={!into || run.isPending}
            onClick={() => {
              setConfirm(false);
              run.mutate(true);
            }}
          >
            Preview
          </Button>
          <Button
            disabled={!previewed || run.isPending}
            onClick={() => (confirm ? run.mutate(false) : setConfirm(true))}
          >
            {run.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : <Merge className="mr-1.5 h-4 w-4" />}
            {confirm ? "Confirm merge" : "Merge"}
          </Button>
        </div>
        {run.error && (
          <p className="text-sm text-destructive" role="alert">
            {apiErrorMessage(run.error)}
          </p>
        )}
        {run.data && <OperationOutcome outcome={run.data} onFinished={finished} />}
      </CardContent>
    </Card>
  );
}

/** The categories a delete lets the administrator choose for. */
function choiceCategories(categories: CategoryReport[]): CategoryReport[] {
  return categories.filter((c) => c.rules.delete.rule === "choice" && c.count > 0);
}

function DeleteCard({ subject, holdings }: { subject: string; holdings: CategoryReport[] }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [recipient, setRecipient] = useState("");
  const [choices, setChoices] = useState<Record<string, LifecycleChoice>>({});
  const [confirm, setConfirm] = useState(false);
  const run = useMutation({
    mutationFn: (dryRun: boolean) =>
      identitiesApi.deleteWithPlan(subject, {
        recipient: recipient || undefined,
        choices,
        dry_run: dryRun,
      }),
  });
  const finished = (report: LifecycleReport) => {
    qc.invalidateQueries({ queryKey: ["identities"] });
    if (report.complete) navigate("/users");
  };
  const reset = () => {
    run.reset();
    setConfirm(false);
  };
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Delete this identity</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">
          Removes everything this person owns. Billing and audit records are kept. Shared
          items others rely on go to a recipient unless you choose to erase them.
        </p>
        <div className="flex flex-wrap items-center gap-2">
          <RecipientSelect
            subject={subject}
            value={recipient}
            onChange={(id) => {
              setRecipient(id);
              reset();
            }}
            placeholder="No recipient"
            ariaLabel="Recipient of transferred items"
          />
          {choiceCategories(holdings).map((c) => (
            <label key={c.category} className="flex items-center gap-1 text-sm">
              {label(c.category)}
              <select
                aria-label={`What happens to ${label(c.category)}`}
                className={SELECT}
                value={choices[c.category] ?? (c.rules.delete.rule === "choice" ? c.rules.delete.default : "erase")}
                onChange={(e) => {
                  setChoices({ ...choices, [c.category]: e.target.value as LifecycleChoice });
                  reset();
                }}
              >
                <option value="erase">erase</option>
                <option value="transfer">transfer</option>
              </select>
            </label>
          ))}
        </div>
        <div className="flex gap-2">
          <Button
            variant="outline"
            disabled={run.isPending}
            onClick={() => {
              setConfirm(false);
              run.mutate(true);
            }}
          >
            Preview
          </Button>
          <Button
            variant="destructive"
            disabled={!isPreview(run.data) || run.isPending}
            onClick={() => (confirm ? run.mutate(false) : setConfirm(true))}
          >
            {run.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : <Trash2 className="mr-1.5 h-4 w-4" />}
            {confirm ? "Confirm delete" : "Delete"}
          </Button>
        </div>
        {run.error && (
          <p className="text-sm text-destructive" role="alert">
            {apiErrorMessage(run.error)}
          </p>
        )}
        {run.data && <OperationOutcome outcome={run.data} onFinished={finished} />}
      </CardContent>
    </Card>
  );
}

function TransferRow({
  subject,
  label: itemLabel,
  item,
}: {
  subject: string;
  label: string;
  item: TransferItem;
}) {
  const qc = useQueryClient();
  const [to, setTo] = useState("");
  const run = useMutation({
    mutationFn: () => identitiesApi.transfer(subject, item, to),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["identities", subject, "holdings"] }),
  });
  return (
    <li className="flex flex-wrap items-center gap-2 text-sm">
      <span className="min-w-0 flex-1 break-all">{itemLabel}</span>
      <RecipientSelect
        subject={subject}
        value={to}
        onChange={setTo}
        placeholder="Transfer to…"
        ariaLabel={`Transfer ${itemLabel} to`}
      />
      <Button size="sm" variant="outline" disabled={!to || run.isPending} onClick={() => run.mutate()}>
        <ArrowRightLeft className="mr-1 h-3 w-3" />
        Transfer
      </Button>
      {run.error && <span className="text-destructive">{apiErrorMessage(run.error)}</span>}
    </li>
  );
}

/** The identity page's lifecycle tab: what the identity holds, and merging,
 * transferring from and deleting it. Admin only; the server refuses anyone else. */
export function IdentityLifecycle({ identityId }: { identityId: string }) {
  const holdings = useQuery({
    queryKey: ["identities", identityId, "holdings"],
    queryFn: () => identitiesApi.holdings(identityId),
  });
  const operations = useQuery({
    queryKey: ["identities", identityId, "operations"],
    queryFn: () => identitiesApi.operations(identityId),
  });

  if (holdings.isLoading) return <Loader2 className="h-5 w-5 animate-spin" />;
  if (holdings.error || !holdings.data) {
    return (
      <p className="text-sm text-destructive" role="alert">
        {apiErrorMessage(holdings.error)}
      </p>
    );
  }
  const categories = holdings.data.categories;
  const movable = categories.flatMap((c) => c.movable ?? []);

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <CardTitle className="text-base">What this identity holds</CardTitle>
        </CardHeader>
        <CardContent className="overflow-x-auto">
          <HoldingsTable categories={categories} />
        </CardContent>
      </Card>
      {movable.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Transfer one item</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="mb-3 text-sm text-muted-foreground">
              A transferred item counts as edited by you: it runs without the new owner's
              private context until they save it themselves.
            </p>
            <ul className="space-y-2">
              {movable.map((m) => (
                <TransferRow key={m.label} subject={identityId} label={m.label} item={m.item} />
              ))}
            </ul>
          </CardContent>
        </Card>
      )}
      <MergeCard subject={identityId} />
      <DeleteCard subject={identityId} holdings={categories} />
      {(operations.data?.operations.length ?? 0) > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Lifecycle operations</CardTitle>
          </CardHeader>
          <CardContent>
            <ul className="space-y-1 text-sm">
              {operations.data?.operations.map((op) => (
                <li key={op.id}>
                  {op.kind} — {op.status} — {new Date(op.started_at).toLocaleString()}
                </li>
              ))}
            </ul>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
