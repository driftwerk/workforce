import { useState } from "react";
import { PiggyBank, ShieldAlert, Trash2 } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useBudgetStatus, useClearOverride, usePutOverride } from "@/hooks/use-budget";
import type { ScopeStatus } from "@/lib/api/budget";

const EXPIRY_PRESETS: { label: string; value: string | number | undefined }[] = [
  { label: "Today", value: "today" },
  { label: "24h", value: 24 },
  { label: "7d", value: 24 * 7 },
  { label: "No expiry", value: undefined },
];

function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function money(v: number): string {
  return `$${v.toFixed(2)}`;
}

function UsageBar({ status }: { status: ScopeStatus }) {
  if (status.unlimited) {
    return <span className="text-xs text-muted-foreground">unlimited</span>;
  }
  const ratio = Math.min(status.spent_usd / Math.max(status.limit_usd, 0.01), 1);
  const tone =
    ratio >= 1 ? "bg-red-500" : ratio >= 0.8 ? "bg-amber-500" : "bg-emerald-500";
  return (
    <div className="h-2 w-full max-w-40 overflow-hidden rounded bg-muted">
      <div className={`h-full ${tone}`} style={{ width: `${ratio * 100}%` }} />
    </div>
  );
}

function ScopeRow({
  status,
  label,
  onClear,
}: {
  status: ScopeStatus;
  label: string;
  onClear?: () => void;
}) {
  const remaining = status.unlimited
    ? null
    : Math.max(status.limit_usd - status.spent_usd, 0);
  return (
    <tr className="border-b border-border/50 last:border-0">
      <td className="py-2 pr-4 font-medium">{label}</td>
      <td className="py-2 pr-4 tabular-nums">{money(status.spent_usd)}</td>
      <td className="py-2 pr-4 tabular-nums">
        {status.unlimited ? "—" : money(status.limit_usd)}
      </td>
      <td className="py-2 pr-4 tabular-nums">
        {remaining == null ? "—" : money(remaining)}
      </td>
      <td className="py-2 pr-4">
        <UsageBar status={status} />
      </td>
      <td className="py-2">
        {status.overridden && (
          <span className="inline-flex items-center gap-1">
            <Badge variant="secondary">
              override
              {status.override_expires_at
                ? ` until ${new Date(status.override_expires_at).toLocaleString()}`
                : ""}
            </Badge>
            {onClear && (
              <Button variant="ghost" size="icon" className="h-6 w-6" onClick={onClear}>
                <Trash2 className="h-3 w-3" />
              </Button>
            )}
          </span>
        )}
      </td>
    </tr>
  );
}

function ScopeTable({
  rows,
  names,
  scope,
  onClear,
}: {
  rows: ScopeStatus[];
  names?: Record<string, string>;
  scope: "agent" | "identity";
  onClear: (scope: string, key: string) => void;
}) {
  return (
    <table className="w-full text-sm">
      <thead>
        <tr className="border-b text-left text-xs uppercase text-muted-foreground">
          <th className="py-2 pr-4">{scope === "agent" ? "Agent" : "User"}</th>
          <th className="py-2 pr-4">Spent today</th>
          <th className="py-2 pr-4">Limit</th>
          <th className="py-2 pr-4">Remaining</th>
          <th className="py-2 pr-4">Usage</th>
          <th className="py-2">Override</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <ScopeRow
            key={r.key}
            status={r}
            label={names?.[r.key] ?? r.key}
            onClear={() => onClear(scope, r.key)}
          />
        ))}
      </tbody>
    </table>
  );
}

function OverrideForm() {
  const put = usePutOverride();
  const [scope, setScope] = useState<"hub" | "agent" | "identity">("identity");
  const [key, setKey] = useState("");
  const [amount, setAmount] = useState("");
  const [expiry, setExpiry] = useState<string | number | undefined>("today");
  const [note, setNote] = useState("");

  const amountNum = Number.parseFloat(amount);
  const valid =
    Number.isFinite(amountNum) && amountNum >= 0 && (scope === "hub" || key.trim() !== "");

  return (
    <form
      className="flex flex-wrap items-end gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        if (!valid) return;
        put.mutate(
          {
            scope,
            key: scope === "hub" ? "" : key.trim(),
            daily_usd: amountNum,
            expires: expiry,
            note: note.trim() || undefined,
          },
          { onSuccess: () => setNote("") },
        );
      }}
    >
      <label className="flex flex-col gap-1 text-xs text-muted-foreground">
        Scope
        <select
          className="h-9 rounded-md border border-input bg-background px-2 text-sm text-foreground"
          value={scope}
          onChange={(e) => setScope(e.target.value as typeof scope)}
        >
          <option value="hub">Workspace</option>
          <option value="agent">Agent</option>
          <option value="identity">User</option>
        </select>
      </label>
      {scope !== "hub" && (
        <label className="flex flex-col gap-1 text-xs text-muted-foreground">
          {scope === "agent" ? "Agent id" : "Identity uuid"}
          <Input
            className="h-9 w-64"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            placeholder={scope === "agent" ? "example" : "8f14e45f-…"}
          />
        </label>
      )}
      <label className="flex flex-col gap-1 text-xs text-muted-foreground">
        Daily USD (0 = unlimited)
        <Input
          className="h-9 w-32"
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
          placeholder="150"
          inputMode="decimal"
        />
      </label>
      <label className="flex flex-col gap-1 text-xs text-muted-foreground">
        Expires
        <select
          className="h-9 rounded-md border border-input bg-background px-2 text-sm text-foreground"
          value={String(expiry)}
          onChange={(e) => {
            const preset = EXPIRY_PRESETS.find((p) => String(p.value) === e.target.value);
            setExpiry(preset?.value);
          }}
        >
          {EXPIRY_PRESETS.map((p) => (
            <option key={p.label} value={String(p.value)}>
              {p.label}
            </option>
          ))}
        </select>
      </label>
      <label className="flex flex-1 basis-64 flex-col gap-1 text-xs text-muted-foreground">
        Note
        <Input
          className="h-9"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="why the boost was granted"
        />
      </label>
      <Button type="submit" disabled={!valid || put.isPending}>
        {put.isPending ? "Saving…" : "Set override"}
      </Button>
      {put.isError && (
        <span className="text-xs text-red-500">{errMsg(put.error)}</span>
      )}
    </form>
  );
}

export default function BudgetPage() {
  const { data, isLoading, isError, error } = useBudgetStatus();
  const clear = useClearOverride();

  if (isLoading) return <PageSkeleton />;
  if (isError || !data) {
    return (
      <EmptyState
        icon={ShieldAlert}
        title="Budget status unavailable"
        description={String(error ?? "unknown error")}
      />
    );
  }

  const onClear = (scope: string, key: string) => clear.mutate({ scope, key });

  return (
    <div className="space-y-6 p-6">
      <div className="flex items-center gap-2">
        <PiggyBank className="h-5 w-5" />
        <h1 className="text-lg font-semibold">Budget</h1>
        <span className="text-sm text-muted-foreground">
          {data.day} — spend figures update within ~30s
        </span>
        {!data.enabled && <Badge variant="destructive">governance disabled</Badge>}
        {clear.isError && (
          <span className="text-xs text-red-500">
            clear failed: {errMsg(clear.error)}
          </span>
        )}
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Workspace</CardTitle>
        </CardHeader>
        <CardContent>
          <table className="w-full text-sm">
            <tbody>
              <ScopeRow
                status={data.hub}
                label="All agents combined"
                onClear={() => onClear("hub", "")}
              />
            </tbody>
          </table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Agents</CardTitle>
        </CardHeader>
        <CardContent className="overflow-x-auto">
          <ScopeTable rows={data.agents} scope="agent" onClear={onClear} />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Users with spend today</CardTitle>
        </CardHeader>
        <CardContent className="overflow-x-auto">
          {data.identities.length === 0 ? (
            <p className="text-sm text-muted-foreground">No user spend recorded today.</p>
          ) : (
            <ScopeTable
              rows={data.identities}
              names={data.identity_names}
              scope="identity"
              onClear={onClear}
            />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Set a runtime override</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <OverrideForm />
          <p className="text-xs text-muted-foreground">
            Overrides replace the configured ceiling for their target, take effect within
            a few seconds, and are audited. Expiring overrides revert automatically —
            prefer “Today” for one-off boosts.
          </p>
        </CardContent>
      </Card>

      {data.overrides.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Active overrides</CardTitle>
          </CardHeader>
          <CardContent className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b text-left text-xs uppercase text-muted-foreground">
                  <th className="py-2 pr-4">Target</th>
                  <th className="py-2 pr-4">Daily limit</th>
                  <th className="py-2 pr-4">Expires</th>
                  <th className="py-2 pr-4">By</th>
                  <th className="py-2 pr-4">Note</th>
                  <th className="py-2"></th>
                </tr>
              </thead>
              <tbody>
                {data.overrides.map((o) => (
                  <tr key={`${o.scope}:${o.key}`} className="border-b border-border/50 last:border-0">
                    <td className="py-2 pr-4">
                      {o.scope === "hub"
                        ? "workspace"
                        : `${o.scope} ${data.identity_names[o.key] ?? o.key}`}
                    </td>
                    <td className="py-2 pr-4 tabular-nums">{money(o.daily_usd)}</td>
                    <td className="py-2 pr-4 whitespace-nowrap">
                      {o.expires_at ? new Date(o.expires_at).toLocaleString() : "until cleared"}
                    </td>
                    <td className="py-2 pr-4">{data.identity_names[o.created_by] ?? o.created_by}</td>
                    <td className="py-2 pr-4">{o.note ?? "—"}</td>
                    <td className="py-2">
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-6 w-6"
                        onClick={() => onClear(o.scope, o.key)}
                      >
                        <Trash2 className="h-3 w-3" />
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}

      {data.audit.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Recent override changes</CardTitle>
          </CardHeader>
          <CardContent className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b text-left text-xs uppercase text-muted-foreground">
                  <th className="py-2 pr-4">When</th>
                  <th className="py-2 pr-4">Action</th>
                  <th className="py-2 pr-4">Target</th>
                  <th className="py-2 pr-4">Amount</th>
                  <th className="py-2">By</th>
                </tr>
              </thead>
              <tbody>
                {data.audit.map((a, i) => (
                  <tr key={i} className="border-b border-border/50 last:border-0">
                    <td className="py-2 pr-4 whitespace-nowrap">
                      {new Date(a.at).toLocaleString()}
                    </td>
                    <td className="py-2 pr-4">
                      <Badge variant={a.action === "set" ? "secondary" : "outline"}>
                        {a.action}
                      </Badge>
                    </td>
                    <td className="py-2 pr-4">
                      {a.scope === "hub"
                        ? "workspace"
                        : `${a.scope} ${data.identity_names[a.key] ?? a.key}`}
                    </td>
                    <td className="py-2 pr-4 tabular-nums">
                      {a.daily_usd == null ? "—" : money(a.daily_usd)}
                    </td>
                    <td className="py-2">{data.identity_names[a.actor] ?? a.actor}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
