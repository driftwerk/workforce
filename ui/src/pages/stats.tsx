import { useState } from "react";
import { Link, useNavigate } from "react-router";
import { AlertCircle } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  DateRangePicker,
  defaultRange,
  type DateRange,
} from "@/components/ui/date-range-picker";
import { DataTable, type Column } from "@/components/ui/data-table";
import { TraceDrawer } from "@/components/stats/trace-drawer";
import { StatCard } from "@/components/stats/stat-card";
import { TokenUsageChart } from "@/components/stats/token-usage-chart";
import { CostBarChart, type CostBarRow } from "@/components/stats/cost-bar-chart";
import { PrincipalPill } from "@/components/principal-pill";
import {
  useStatsOverview,
  useCostBreakdown,
  useStatsIdentities,
  useStatsAgents,
  useStatsModels,
  useStatsTools,
  useStatsEfficiency,
} from "@/hooks/use-stats";
import { useAuthStore } from "@/stores/auth-store";
import type {
  GroupedRow,
  TaskHistoryEntry,
  ToolBreakdown,
} from "@/lib/api/stats";
import { formatCost, formatDuration } from "@/lib/utils";
import { BILLING_SERIES, billingBarRow, originLabel } from "@/lib/cost-billing";

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function toBarRows(rows: GroupedRow[]): CostBarRow[] {
  return rows.slice(0, 10).map((r) => ({
    label: r.display_name ?? r.key,
    cost: r.cost_usd,
    raw: r,
  }));
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error ?? "Unknown error");
}

function TabError({ error }: { error: unknown }) {
  return (
    <div className="mt-4 flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
      <AlertCircle className="h-4 w-4" />
      <span>Failed to load: {errorMessage(error)}</span>
    </div>
  );
}

export default function StatsPage() {
  const [range, setRange] = useState<DateRange>(defaultRange());
  const [trace, setTrace] = useState<string | null>(null);
  const navigate = useNavigate();
  const identity = useAuthStore((s) => s.identity);
  const isAdmin = identity?.role === "system_admin" || identity?.role === "manager";

  if (identity && !isAdmin) {
    // Members see their own usage on the user-detail page rather than the
    // global leaderboards.
    navigate(`/users/${identity.id}?tab=usage`, { replace: true });
    return null;
  }

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold">Stats</h1>
          <p className="text-muted-foreground">
            Cost, usage, and efficiency analytics across the workforce.
          </p>
        </div>
        <DateRangePicker value={range} onChange={setRange} />
      </div>

      <Tabs defaultValue="overview">
        <TabsList>
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="cost">Cost breakdown</TabsTrigger>
          <TabsTrigger value="identities">Identities</TabsTrigger>
          <TabsTrigger value="agents">Agents</TabsTrigger>
          <TabsTrigger value="models">Models</TabsTrigger>
          <TabsTrigger value="tools">Tools</TabsTrigger>
          <TabsTrigger value="efficiency">Efficiency</TabsTrigger>
        </TabsList>

        <TabsContent value="overview">
          <OverviewTab range={range} />
        </TabsContent>
        <TabsContent value="cost">
          <CostBreakdownTab range={range} />
        </TabsContent>
        <TabsContent value="identities">
          <IdentitiesTab range={range} />
        </TabsContent>
        <TabsContent value="agents">
          <AgentsTab range={range} />
        </TabsContent>
        <TabsContent value="models">
          <ModelsTab range={range} />
        </TabsContent>
        <TabsContent value="tools">
          <ToolsTab range={range} />
        </TabsContent>
        <TabsContent value="efficiency">
          <EfficiencyTab range={range} onTraceTask={setTrace} />
        </TabsContent>
      </Tabs>

      <TraceDrawer taskId={trace} onClose={() => setTrace(null)} />
    </div>
  );
}

function OverviewTab({ range }: { range: DateRange }) {
  const { data, isLoading } = useStatsOverview(range);
  return (
    <div className="space-y-6 mt-4">
      <div className="grid gap-4 md:grid-cols-4">
        <StatCard label="Cost (USD)" value={formatCost(data?.totals.cost_usd ?? 0)} isLoading={isLoading} />
        <StatCard label="Tasks" value={(data?.totals.task_count ?? 0).toLocaleString()} isLoading={isLoading} />
        <StatCard label="Input tokens" value={formatTokens(data?.totals.input_tokens ?? 0)} isLoading={isLoading} />
        <StatCard label="Output tokens" value={formatTokens(data?.totals.output_tokens ?? 0)} isLoading={isLoading} />
      </div>

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Token usage over time</CardTitle>
        </CardHeader>
        <CardContent>
          <TokenUsageChart data={data?.history ?? []} gradientId="overview" />
        </CardContent>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <CostBarChart title="Top identities" rows={toBarRows(data?.by_identity ?? [])} />
        <CostBarChart title="Top agents" rows={toBarRows(data?.by_agent ?? [])} />
        <CostBarChart title="Top models" rows={toBarRows(data?.by_model ?? [])} />
        <CostBarChart title="By provider" rows={toBarRows(data?.by_provider ?? [])} />
      </div>
    </div>
  );
}

function CostBreakdownTab({ range }: { range: DateRange }) {
  const navigate = useNavigate();
  const { data } = useCostBreakdown(range);
  const onIdentityClick = (row: CostBarRow) => {
    const r = row.raw as GroupedRow;
    if (r.key !== "unknown") navigate(`/users/${r.key}?tab=usage`);
  };
  return (
    <div className="space-y-6 mt-4">
      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Where did the money go?</CardTitle>
        </CardHeader>
        <CardContent>
          <TokenUsageChart data={data?.history ?? []} gradientId="cost-breakdown" />
        </CardContent>
      </Card>
      <div className="grid gap-4 lg:grid-cols-2">
        <CostBarChart
          title="By identity"
          rows={toBarRows(data?.by_identity ?? [])}
          onRowClick={onIdentityClick}
        />
        <CostBarChart title="By agent" rows={toBarRows(data?.by_agent ?? [])} />
        <CostBarChart
          title="Metered vs subscription by agent"
          series={BILLING_SERIES}
          rows={(data?.by_agent_billing ?? [])
            .slice(0, 10)
            .map((r) => billingBarRow(r.display_name ?? r.agent_id, r.split))}
        />
        <CostBarChart
          title="By origin"
          series={BILLING_SERIES}
          rows={(data?.by_origin ?? []).map((r) => billingBarRow(originLabel(r.origin), r.split))}
        />
        <CostBarChart title="By model" rows={toBarRows(data?.by_model ?? [])} />
        <CostBarChart title="By provider" rows={toBarRows(data?.by_provider ?? [])} />
        <CostBarChart title="By task type" rows={toBarRows(data?.by_task_type ?? [])} />
        <CostBarChart title="By team" rows={toBarRows(data?.by_team ?? [])} />
      </div>
      {data && data.by_ticket.length > 0 && (
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">By ticket</CardTitle>
          </CardHeader>
          <CardContent>
            <DataTable<GroupedRow>
              columns={[
                { key: "ticket", header: "Ticket", cell: (r) => r.key },
                { key: "cost", header: "Cost", align: "right", cell: (r) => formatCost(r.cost_usd) },
              ]}
              rows={data.by_ticket}
              rowKey={(r) => r.key}
            />
          </CardContent>
        </Card>
      )}
    </div>
  );
}

function IdentitiesTab({ range }: { range: DateRange }) {
  const navigate = useNavigate();
  const [cursor, setCursor] = useState<string | undefined>();
  const { data, isLoading, error } = useStatsIdentities(range, { cursor, limit: 50 });
  const cols: Column<GroupedRow>[] = [
    {
      key: "name",
      header: "Identity",
      cell: (r) => (
        <Link to={`/users/${r.key}?tab=usage`} className="hover:text-primary">
          <PrincipalPill principal={{ id: r.key, display_name: r.display_name }} />
        </Link>
      ),
    },
    { key: "cost", header: "Cost", align: "right", cell: (r) => formatCost(r.cost_usd) },
  ];
  if (error) return <TabError error={error} />;
  return (
    <div className="mt-4">
      <DataTable<GroupedRow>
        columns={cols}
        rows={data?.entries ?? []}
        rowKey={(r) => r.key}
        isLoading={isLoading}
        onRowClick={(r) => navigate(`/users/${r.key}?tab=usage`)}
        footer={
          data?.next_cursor && (
            <button
              onClick={() => setCursor(data.next_cursor!)}
              className="rounded-md px-3 py-1 hover:bg-muted"
            >
              Load more →
            </button>
          )
        }
      />
    </div>
  );
}

function AgentsTab({ range }: { range: DateRange }) {
  const navigate = useNavigate();
  const [cursor, setCursor] = useState<string | undefined>();
  const { data, isLoading, error } = useStatsAgents(range, { cursor, limit: 50 });
  const cols: Column<GroupedRow>[] = [
    {
      key: "agent",
      header: "Agent",
      cell: (r) => (
        <Link to={`/agents/${r.key}`} className="hover:text-primary">
          {r.display_name ?? r.key}
        </Link>
      ),
    },
    { key: "cost", header: "Cost", align: "right", cell: (r) => formatCost(r.cost_usd) },
  ];
  if (error) return <TabError error={error} />;
  return (
    <div className="mt-4">
      <DataTable<GroupedRow>
        columns={cols}
        rows={data?.entries ?? []}
        rowKey={(r) => r.key}
        isLoading={isLoading}
        onRowClick={(r) => navigate(`/agents/${r.key}`)}
        footer={
          data?.next_cursor && (
            <button
              onClick={() => setCursor(data.next_cursor!)}
              className="rounded-md px-3 py-1 hover:bg-muted"
            >
              Load more →
            </button>
          )
        }
      />
    </div>
  );
}

function ModelsTab({ range }: { range: DateRange }) {
  const { data } = useStatsModels(range);
  return (
    <div className="mt-4">
      <CostBarChart title="Cost by model" rows={toBarRows(data ?? [])} />
    </div>
  );
}

function ToolsTab({ range }: { range: DateRange }) {
  const { data, isLoading, error } = useStatsTools(range, 50);
  const cols: Column<ToolBreakdown>[] = [
    { key: "tool", header: "Tool", cell: (r) => <span className="font-mono">{r.tool_name}</span> },
    { key: "calls", header: "Calls", align: "right", cell: (r) => r.invocations.toLocaleString() },
    { key: "success", header: "Success", align: "right", cell: (r) => `${r.success_rate_pct.toFixed(1)}%` },
    { key: "duration", header: "Avg time", align: "right", cell: (r) => formatDuration(r.avg_duration_ms) },
  ];
  if (error) return <TabError error={error} />;
  return (
    <div className="mt-4">
      <DataTable<ToolBreakdown>
        columns={cols}
        rows={data ?? []}
        rowKey={(r) => r.tool_name}
        isLoading={isLoading}
        empty="No tool calls in range."
      />
    </div>
  );
}

function EfficiencyTab({
  range,
  onTraceTask,
}: {
  range: DateRange;
  onTraceTask: (id: string) => void;
}) {
  const { data, isLoading, error } = useStatsEfficiency(range);
  const cols: Column<TaskHistoryEntry>[] = [
    { key: "agent", header: "Agent", cell: (r) => r.agent_id },
    { key: "model", header: "Model", cell: (r) => <span className="font-mono">{r.model}</span> },
    { key: "rounds", header: "Rounds", align: "right", cell: (r) => r.rounds },
    { key: "tools", header: "Tools", align: "right", cell: (r) => r.tool_call_count },
    { key: "cost", header: "Cost", align: "right", cell: (r) => formatCost(r.cost_usd) },
    {
      key: "completed",
      header: "When",
      cell: (r) => new Date(r.completed_at).toLocaleString(),
    },
  ];
  if (error) return <TabError error={error} />;
  return (
    <div className="mt-4">
      <DataTable<TaskHistoryEntry>
        columns={cols}
        rows={data?.outliers ?? []}
        rowKey={(r) => r.id}
        isLoading={isLoading}
        empty="No efficiency outliers in range."
        onRowClick={(r) => r.task_id && onTraceTask(r.task_id)}
      />
    </div>
  );
}
