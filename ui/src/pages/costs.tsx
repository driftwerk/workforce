import { useState, useMemo } from "react";
import { DollarSign, RefreshCw } from "lucide-react";
import {
  Bar,
  BarChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { PrincipalPill } from "@/components/principal-pill";
import { useCostDetail, useCostMonthly } from "@/hooks/use-cost";
import { formatCostScaled, formatTokens } from "@/lib/utils";
import type { Billing, BillingSplit, CostDetailRecord, MonthlyPoint } from "@/lib/api/cost";
import { billedCost, billingLabel, ESTIMATE_TITLE } from "@/lib/cost-billing";

interface TimeRange {
  readonly label: string;
  readonly hours: number | null;
}

// "All": effectively unbounded; we anchor to 1970 rather than synthesising
// a future-proof sentinel.
const TIME_RANGES: readonly TimeRange[] = [
  { label: "1h", hours: 1 },
  { label: "6h", hours: 6 },
  { label: "24h", hours: 24 },
  { label: "7d", hours: 24 * 7 },
  { label: "30d", hours: 24 * 30 },
  { label: "All", hours: null },
];

function startFromRange(range: TimeRange): string {
  if (range.hours == null) return new Date(0).toISOString();
  return new Date(Date.now() - range.hours * 60 * 60 * 1000).toISOString();
}

const MONTH_ABBR = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun",
  "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTH_LONG = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

function MonthlyBreakdownChart({ points }: { points: MonthlyPoint[] }) {
  const data = points.map((p) => ({
    month: p.month.slice(0, 7),
    cost: p.cost_usd,
    tokens: p.input_tokens + p.output_tokens,
  }));
  const hasData = data.some((d) => d.cost > 0);

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Monthly Cost (past 12 months)</CardTitle>
      </CardHeader>
      <CardContent>
        {!hasData ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            No cost data for this period.
          </div>
        ) : (
          <ResponsiveContainer width="100%" height={200}>
            <BarChart data={data} margin={{ top: 4, right: 8, left: 8, bottom: 4 }}>
              <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
              <XAxis
                dataKey="month"
                tick={{ fontSize: 11 }}
                tickFormatter={(v: string) => {
                  const [y, m] = v.split("-");
                  const idx = Number(m) - 1;
                  return `${MONTH_ABBR[idx] ?? ""} ${y?.slice(2) ?? ""}`;
                }}
              />
              <YAxis
                tick={{ fontSize: 11 }}
                tickFormatter={(v: number) => formatCostScaled(v)}
                width={60}
              />
              <Tooltip
                formatter={(v: unknown) => [formatCostScaled(Number(v ?? 0)), "Cost"]}
                labelFormatter={(label) => {
                  const s = String(label);
                  const [y, m] = s.split("-");
                  const idx = Number(m) - 1;
                  return `${MONTH_LONG[idx] ?? ""} ${y ?? ""}`;
                }}
                contentStyle={{
                  backgroundColor: "hsl(var(--card))",
                  border: "1px solid hsl(var(--border))",
                  borderRadius: "0.5rem",
                  fontSize: "0.75rem",
                }}
              />
              <Bar dataKey="cost" fill="hsl(221, 83%, 53%)" radius={[3, 3, 0, 0]} />
            </BarChart>
          </ResponsiveContainer>
        )}
      </CardContent>
    </Card>
  );
}

function SummaryCards({
  records,
  total,
  billing,
}: {
  records: CostDetailRecord[];
  total: number;
  billing: BillingSplit | undefined;
}) {
  const pageTokens = records.reduce((s, r) => s + r.input_tokens + r.output_tokens, 0);

  const stats = [
    {
      label: "Metered spend",
      value: billedCost(billing?.metered_cost_usd ?? 0, "metered"),
      sub: "billed per token",
    },
    {
      label: "Subscription value (est.)",
      value: billedCost(billing?.subscription_cost_usd ?? 0, "subscription"),
      sub: `${(billing?.subscription_calls ?? 0).toLocaleString()} calls on a plan`,
      title: ESTIMATE_TITLE,
    },
    { label: "Calls", value: total.toLocaleString(), sub: "matching the filters" },
    { label: "Tokens", value: formatTokens(pageTokens), sub: "input + output, this page" },
  ];

  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
      {stats.map((s) => (
        <Card key={s.label} title={s.title}>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground">
              {s.label}
            </CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-2xl font-bold truncate">{s.value}</p>
            <p className="text-xs text-muted-foreground mt-1">{s.sub}</p>
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

function CostTable({ records }: { records: CostDetailRecord[] }) {
  if (records.length === 0) {
    return (
      <EmptyState
        icon={DollarSign}
        title="No records"
        description="No LLM calls match the current filters."
      />
    );
  }

  const columns = ["Time", "Agent", "User", "Model", "Type", "Billing", "Description", "Tokens", "Cost"];

  return (
    <div className="overflow-x-auto rounded-lg border">
      <table className="w-full text-sm">
        <thead className="bg-muted/50">
          <tr>
            {columns.map((h) => (
              <th
                key={h}
                className="px-3 py-2 text-left text-xs font-medium text-muted-foreground whitespace-nowrap"
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y divide-border">
          {records.map((r) => (
            <tr key={r.id} className="hover:bg-muted/30 transition-colors">
              <td className="px-3 py-2 whitespace-nowrap text-xs text-muted-foreground">
                {new Date(r.timestamp).toLocaleString(undefined, {
                  month: "short",
                  day: "numeric",
                  hour: "2-digit",
                  minute: "2-digit",
                  second: "2-digit",
                })}
              </td>
              <td className="px-3 py-2 whitespace-nowrap">
                <Badge variant="outline" className="text-xs">
                  {r.agent_display_name || r.agent_id}
                </Badge>
              </td>
              <td className="px-3 py-2 whitespace-nowrap text-xs">
                <PrincipalPill
                  principal={
                    r.user_id
                      ? {
                          id: r.user_id,
                          display_name: r.user_display_name,
                          kind: r.user_kind ?? "user",
                        }
                      : null
                  }
                />
              </td>
              <td className="px-3 py-2 text-xs max-w-[140px] truncate" title={r.model}>
                {r.model || <span className="text-muted-foreground italic">none</span>}
              </td>
              <td className="px-3 py-2 whitespace-nowrap">
                <span className="text-xs text-muted-foreground">{r.task_type ?? "—"}</span>
              </td>
              <td className="px-3 py-2 whitespace-nowrap text-xs">{billingLabel(r)}</td>
              <td className="px-3 py-2 max-w-[260px]">
                {r.description ? (
                  <span
                    className="text-xs text-muted-foreground truncate block"
                    title={r.description}
                  >
                    {r.description}
                  </span>
                ) : (
                  <span className="text-xs text-muted-foreground italic">—</span>
                )}
              </td>
              <td className="px-3 py-2 whitespace-nowrap text-xs">
                <span
                  title={`in:${r.input_tokens} out:${r.output_tokens} cache_read:${r.cache_read_input_tokens}`}
                >
                  {formatTokens(r.input_tokens + r.output_tokens)}
                </span>
              </td>
              <td className="px-3 py-2 whitespace-nowrap text-xs font-mono">
                {r.cost_usd > 0 ? (
                  <span
                    className="text-amber-600 dark:text-amber-400"
                    title={r.billing === "subscription" ? ESTIMATE_TITLE : undefined}
                  >
                    {billedCost(r.cost_usd, r.billing)}
                  </span>
                ) : (
                  <span className="text-muted-foreground">—</span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

const PAGE_SIZE = 100;

export default function CostsPage() {
  const [rangeIdx, setRangeIdx] = useState(2);
  const [agentFilter, setAgentFilter] = useState("");
  const [userFilter, setUserFilter] = useState("");
  const [billingFilter, setBillingFilter] = useState<Billing | "">("");
  const [page, setPage] = useState(0);

  const range = TIME_RANGES[rangeIdx] ?? TIME_RANGES[0]!;

  const { data: monthlyData } = useCostMonthly(12);
  const monthlyPoints = monthlyData?.points ?? [];

  const params = useMemo(
    () => ({
      start: startFromRange(range),
      agent_id: agentFilter || undefined,
      user_id: userFilter || undefined,
      billing: billingFilter || undefined,
      limit: PAGE_SIZE,
      offset: page * PAGE_SIZE,
    }),
    [range, agentFilter, userFilter, billingFilter, page],
  );

  const { data, isLoading, error, refetch, isFetching } = useCostDetail(params);

  const records = data?.records ?? [];
  const total = data?.total ?? 0;
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const distinctUsers = data?.distinct_users ?? [];
  const distinctAgents = data?.distinct_agents ?? [];

  function handleRangeChange(idx: number) {
    setRangeIdx(idx);
    setPage(0);
  }
  function handleAgentChange(v: string) {
    setAgentFilter(v);
    setPage(0);
  }
  function handleUserChange(v: string) {
    setUserFilter(v);
    setPage(0);
  }
  function handleBillingChange(v: Billing | "") {
    setBillingFilter(v);
    setPage(0);
  }

  if (isLoading && !data) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold">Cost Detail</h1>
          <p className="text-muted-foreground">
            Every LLM call — what was spent, why, and who initiated it.
          </p>
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={() => refetch()}
          disabled={isFetching}
          className="gap-2"
        >
          <RefreshCw className={`h-4 w-4 ${isFetching ? "animate-spin" : ""}`} />
          Refresh
        </Button>
      </div>

      <div className="flex flex-wrap items-center gap-3">
        <div className="flex rounded-md border overflow-hidden">
          {TIME_RANGES.map((r, idx) => (
            <button
              key={r.label}
              onClick={() => handleRangeChange(idx)}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                rangeIdx === idx
                  ? "bg-primary text-primary-foreground"
                  : "bg-background hover:bg-muted text-muted-foreground"
              }`}
            >
              {r.label}
            </button>
          ))}
        </div>

        <select
          aria-label="Filter by agent"
          value={agentFilter}
          onChange={(e) => handleAgentChange(e.target.value)}
          className="h-9 rounded-md border border-input bg-background px-3 py-1.5 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <option value="">All agents</option>
          {distinctAgents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.display_name || a.id}
            </option>
          ))}
        </select>

        <select
          aria-label="Filter by user"
          value={userFilter}
          onChange={(e) => handleUserChange(e.target.value)}
          className="h-9 rounded-md border border-input bg-background px-3 py-1.5 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <option value="">All users</option>
          {distinctUsers.map((u) => (
            <option key={u.id} value={u.id}>
              {u.display_name || u.id}
            </option>
          ))}
        </select>

        <select
          aria-label="Filter by billing"
          value={billingFilter}
          onChange={(e) => handleBillingChange(e.target.value as Billing | "")}
          className="h-9 rounded-md border border-input bg-background px-3 py-1.5 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <option value="">All billing</option>
          <option value="metered">Metered</option>
          <option value="subscription">Subscription</option>
        </select>

        {total > 0 && (
          <span className="text-sm text-muted-foreground ml-auto">
            {total.toLocaleString()} record{total !== 1 ? "s" : ""}
          </span>
        )}
      </div>

      {error && (
        <div className="text-sm text-destructive">
          Failed to load cost data: {error instanceof Error ? error.message : "Unknown error"}
        </div>
      )}

      <MonthlyBreakdownChart points={monthlyPoints} />

      <SummaryCards records={records} total={total} billing={data?.billing_totals} />

      <CostTable records={records} />

      {totalPages > 1 && (
        <div className="flex items-center justify-between text-sm text-muted-foreground">
          <span>
            Page {page + 1} of {totalPages}
          </span>
          <div className="flex gap-2">
            <Button
              variant="outline"
              size="sm"
              disabled={page === 0}
              onClick={() => setPage((p) => p - 1)}
            >
              Previous
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={page >= totalPages - 1}
              onClick={() => setPage((p) => p + 1)}
            >
              Next
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
