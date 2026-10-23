import { useState } from "react";
import { useParams, useNavigate, useSearchParams, Link } from "react-router";
import {
  ArrowLeft,
  Bot,
  Cpu,
  Wrench,
  Trash2,
  ChevronDown,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import {
  DateRangePicker,
  defaultRange,
  type DateRange,
} from "@/components/ui/date-range-picker";
import { DataTable, type Column } from "@/components/ui/data-table";
import { TraceDrawer } from "@/components/stats/trace-drawer";
import { StatCard } from "@/components/stats/stat-card";
import { TokenUsageChart } from "@/components/stats/token-usage-chart";
import { CostBarChart } from "@/components/stats/cost-bar-chart";
import {
  useAgentStats as useAgentStatsRich,
  useAgentTasks,
} from "@/hooks/use-stats";
import type { PayerRow, TaskHistoryEntry, ToolBreakdown } from "@/lib/api/stats";
import { formatCost, formatDuration } from "@/lib/utils";
import {
  BILLING_SERIES,
  ESTIMATE_TITLE,
  billedCost,
  billingBarRow,
  originLabel,
} from "@/lib/cost-billing";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useAgentDetail, useAgentTools, useAgentStats, useDeleteAgent } from "@/hooks/use-agents";
import { IdentityEditor } from "@/components/agents/identity-editor";
import { AgentConfigEditor } from "@/components/agents/agent-config-editor";
import { AgentVault } from "@/components/agents/agent-vault";
import { AgentLogins } from "@/components/agents/agent-logins";
import { AgentSlack } from "@/components/agents/agent-slack";
import { AgentTraining } from "@/components/agents/agent-training";
import { AgentSelfLearning } from "@/components/agents/agent-self-learning";
import { AgentKnowHow } from "@/components/agents/agent-know-how";
import { AgentEnvironments } from "@/components/agents/agent-environments";
import { AgentSettings } from "@/components/agents/agent-settings";
import { SkillManager } from "@/components/agents/skill-manager";
import { SchedulesTab } from "@/pages/scheduler";

const TABS = {
  overview: "Overview",
  usage: "Usage",
  identity: "Identity Files",
  config: "Configuration",
  settings: "Effective settings",
  vault: "Vault",
  logins: "Logins",
  slack: "Slack",
  skills: "Skills",
  training: "Training",
  "self-learning": "Self-Learning",
  "know-how": "Know-how",
  environments: "Environments",
  schedules: "Schedules",
  tools: "Tools",
} as const;

type Tab = keyof typeof TABS;

const tabOf = (requested: string | null): Tab =>
  requested && Object.hasOwn(TABS, requested) ? (requested as Tab) : "overview";

export default function AgentDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [search, setSearch] = useSearchParams();
  const { data: agent, isLoading, error } = useAgentDetail(id);
  const { data: toolsData } = useAgentTools(id);
  const { data: stats } = useAgentStats(id);
  const deleteMutation = useDeleteAgent();

  const [deleteOpen, setDeleteOpen] = useState(false);
  const [expandedTool, setExpandedTool] = useState<string | null>(null);

  if (isLoading) return <PageSkeleton />;

  if (error || !agent) {
    return (
      <div className="p-6">
        <div className="text-destructive">
          Failed to load agent: {error instanceof Error ? error.message : "Agent not found"}
        </div>
      </div>
    );
  }

  const tools = toolsData?.tools ?? [];
  const displayName = agent.name ?? agent.id;

  function handleDelete() {
    if (!id) return;
    deleteMutation.mutate(id, {
      onSuccess: () => {
        setDeleteOpen(false);
        navigate("/agents");
      },
    });
  }

  return (
    <div className="p-6 space-y-6">
      {/* Header */}
      <div className="flex items-start justify-between">
        <div className="flex items-center gap-3">
          <Button variant="ghost" size="sm" onClick={() => navigate("/agents")}>
            <ArrowLeft className="h-4 w-4 mr-1" />Back
          </Button>
          <div className="rounded-full bg-muted p-2">
            <Bot className="h-5 w-5 text-muted-foreground" />
          </div>
          <div>
            <h1 className="text-xl font-bold">{displayName}</h1>
            {agent.name && <p className="text-sm text-muted-foreground font-mono">{agent.id}</p>}
          </div>
          {agent.default && <Badge variant="success">default</Badge>}
        </div>
        <Button variant="outline" size="sm" className="text-destructive hover:text-destructive" onClick={() => setDeleteOpen(true)}>
          <Trash2 className="h-3.5 w-3.5 mr-1" />Delete
        </Button>
      </div>

      {/* Tabs */}
      <Tabs value={tabOf(search.get("tab"))} onValueChange={(tab) => setSearch({ tab }, { replace: true })}>
        <TabsList>
          {Object.entries(TABS).map(([value, label]) => (
            <TabsTrigger key={value} value={value}>
              {label}
            </TabsTrigger>
          ))}
        </TabsList>

        <TabsContent value="usage">
          <AgentUsageTab agentId={agent.id} />
        </TabsContent>

        {/* Overview */}
        <TabsContent value="overview">
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 mt-4">
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-sm font-medium text-muted-foreground">Agent ID</CardTitle>
              </CardHeader>
              <CardContent>
                <p className="text-lg font-mono">{agent.id}</p>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-sm font-medium text-muted-foreground">Model</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="flex items-center gap-2">
                  <Cpu className="h-4 w-4 text-muted-foreground" />
                  <p className="text-lg">{agent.model ?? "default"}</p>
                </div>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-sm font-medium text-muted-foreground">Capabilities</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="flex items-center gap-2">
                  <Wrench className="h-4 w-4 text-muted-foreground" />
                  <p className="text-lg">{agent.capabilities.length} tools</p>
                </div>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-sm font-medium text-muted-foreground">Tool Count</CardTitle>
              </CardHeader>
              <CardContent>
                <p className="text-lg">{agent.tool_count ?? tools.length}</p>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-sm font-medium text-muted-foreground">Sessions (7d)</CardTitle>
              </CardHeader>
              <CardContent>
                <p className="text-lg">{stats?.sessions ?? "--"}</p>
              </CardContent>
            </Card>
            <Card>
              <CardHeader className="pb-2">
                <CardTitle className="text-sm font-medium text-muted-foreground">Default</CardTitle>
              </CardHeader>
              <CardContent>
                <Badge variant={agent.default ? "success" : "secondary"}>
                  {agent.default ? "Yes" : "No"}
                </Badge>
              </CardContent>
            </Card>
            {agent.owner_id && (
              <Card>
                <CardHeader className="pb-2">
                  <CardTitle className="text-sm font-medium text-muted-foreground">
                    {agent.owner_type === "team" ? "Team" : "Owner"}
                  </CardTitle>
                </CardHeader>
                <CardContent>
                  {agent.owner_type === "team" ? (
                    <Button variant="link" className="p-0 h-auto text-sm" onClick={() => navigate(`/teams/${agent.owner_id}`)}>
                      {agent.owner_id.slice(0, 8)}...
                    </Button>
                  ) : (
                    <p className="font-mono text-sm">{agent.owner_id.slice(0, 8)}...</p>
                  )}
                  <p className="text-xs text-muted-foreground">{agent.owner_type ?? "user"}</p>
                </CardContent>
              </Card>
            )}
          </div>
        </TabsContent>

        {/* Identity */}
        <TabsContent value="identity">
          <div className="mt-4">
            <IdentityEditor agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Configuration */}
        <TabsContent value="config">
          <div className="mt-4">
            <AgentConfigEditor agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Effective settings */}
        <TabsContent value="settings">
          <div className="mt-4">
            <AgentSettings agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Vault */}
        <TabsContent value="vault">
          <div className="mt-4">
            <AgentVault agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Logins */}
        <TabsContent value="logins">
          <div className="mt-4">
            <AgentLogins agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Slack */}
        <TabsContent value="slack">
          <div className="mt-4">
            <AgentSlack agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Skills */}
        <TabsContent value="skills">
          <div className="mt-4">
            <SkillManager agentId={agent.id} />
          </div>
        </TabsContent>

        {/* Schedules */}
        <TabsContent value="training">
          <div className="mt-4">
            {id && <AgentTraining agentId={id} />}
          </div>
        </TabsContent>

        <TabsContent value="self-learning">
          <div className="mt-4">
            {id && <AgentSelfLearning agentId={id} />}
          </div>
        </TabsContent>

        <TabsContent value="know-how">
          <div className="mt-4">
            {id && <AgentKnowHow agentId={id} />}
          </div>
        </TabsContent>

        <TabsContent value="environments">
          <div className="mt-4">
            {id && <AgentEnvironments key={id} agentId={id} />}
          </div>
        </TabsContent>

        <TabsContent value="schedules">
          <div className="mt-4">
            <SchedulesTab agentId={id} />
          </div>
        </TabsContent>

        {/* Tools */}
        <TabsContent value="tools">
          <div className="mt-4">
            {tools.length === 0 ? (
              <div className="text-sm text-muted-foreground py-8 text-center">
                No tools registered for this agent.
              </div>
            ) : (
              <div className="border rounded-lg overflow-hidden">
                <table className="w-full">
                  <thead>
                    <tr className="border-b bg-muted/50">
                      <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-8" />
                      <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">Name</th>
                      <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">Description</th>
                    </tr>
                  </thead>
                  <tbody>
                    {tools.map((tool) => (
                      <ToolRow
                        key={tool.name}
                        tool={tool}
                        expanded={expandedTool === tool.name}
                        onToggle={() => setExpandedTool(expandedTool === tool.name ? null : tool.name)}
                      />
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </TabsContent>
      </Tabs>

      {/* Delete confirmation */}
      <Dialog open={deleteOpen} onOpenChange={setDeleteOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Agent</DialogTitle>
            <DialogDescription>
              Are you sure you want to delete agent <span className="font-mono font-semibold">{agent.id}</span>?
              This action cannot be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteOpen(false)}>Cancel</Button>
            <Button variant="destructive" disabled={deleteMutation.isPending} onClick={handleDelete}>
              {deleteMutation.isPending ? "Deleting..." : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function ToolRow({
  tool,
  expanded,
  onToggle,
}: {
  tool: { name: string; description: string; schema?: Record<string, unknown> };
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <>
      <tr className="border-b last:border-b-0 hover:bg-muted/20 transition-colors cursor-pointer" onClick={onToggle}>
        <td className="px-4 py-3">
          <ChevronDown className={`h-4 w-4 text-muted-foreground transition-transform ${expanded ? "rotate-180" : ""}`} />
        </td>
        <td className="px-4 py-3"><span className="text-sm font-medium font-mono">{tool.name}</span></td>
        <td className="px-4 py-3"><span className="text-sm text-muted-foreground">{tool.description}</span></td>
      </tr>
      {expanded && tool.schema && (
        <tr className="border-b bg-muted/10">
          <td colSpan={3} className="px-4 py-3">
            <pre className="text-xs font-mono bg-muted rounded p-3 overflow-auto max-h-64">
              {JSON.stringify(tool.schema, null, 2)}
            </pre>
          </td>
        </tr>
      )}
    </>
  );
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function AgentUsageTab({ agentId }: { agentId: string }) {
  const [range, setRange] = useState<DateRange>(defaultRange());
  const [trace, setTrace] = useState<string | null>(null);
  const [cursor, setCursor] = useState<string | undefined>();
  const stats = useAgentStatsRich(agentId, range);
  const tasks = useAgentTasks(agentId, range, { cursor, limit: 25 });

  const totals = stats.data?.totals;
  const billing = totals?.billing;
  const topModels = (stats.data?.by_model ?? [])
    .slice(0, 8)
    .map((m) => ({ label: m.group_key, cost: m.total_cost }));
  const topIdentities = (stats.data?.by_identity ?? [])
    .slice(0, 8)
    .map((r) => ({ label: r.display_name ?? r.key, cost: r.cost_usd }));

  const taskCols: Column<TaskHistoryEntry>[] = [
    { key: "when", header: "When", cell: (r) => new Date(r.completed_at).toLocaleString() },
    {
      key: "id",
      header: "Identity",
      cell: (r) =>
        r.identity_id ? (
          <Link to={`/users/${r.identity_id}?tab=usage`} className="hover:text-primary">
            {r.identity_id.slice(0, 8)}…
          </Link>
        ) : (
          <span className="italic text-muted-foreground">system</span>
        ),
    },
    { key: "model", header: "Model", cell: (r) => <span className="font-mono text-xs">{r.model}</span> },
    { key: "rounds", header: "Rounds", align: "right", cell: (r) => r.rounds },
    { key: "tools", header: "Tools", align: "right", cell: (r) => r.tool_call_count },
    { key: "cost", header: "Cost", align: "right", cell: (r) => formatCost(r.cost_usd) },
    {
      key: "ok",
      header: "Outcome",
      cell: (r) => (r.success ? <Badge variant="default">ok</Badge> : <Badge variant="destructive">fail</Badge>),
    },
  ];

  const payerCols: Column<PayerRow>[] = [
    {
      key: "plan",
      header: "Plan",
      cell: (r) => r.plan,
    },
    { key: "calls", header: "Calls", align: "right", cell: (r) => r.calls.toLocaleString() },
    {
      key: "cost",
      header: "Value (est.)",
      align: "right",
      cell: (r) => <span title={ESTIMATE_TITLE}>{billedCost(r.cost_usd, "subscription")}</span>,
    },
  ];

  const toolCols: Column<ToolBreakdown>[] = [
    { key: "tool", header: "Tool", cell: (r) => <span className="font-mono">{r.tool_name}</span> },
    { key: "calls", header: "Calls", align: "right", cell: (r) => r.invocations.toLocaleString() },
    { key: "success", header: "Success", align: "right", cell: (r) => `${r.success_rate_pct.toFixed(1)}%` },
    { key: "duration", header: "Avg time", align: "right", cell: (r) => formatDuration(r.avg_duration_ms) },
  ];

  return (
    <div className="space-y-6 mt-4">
      <div className="flex justify-end">
        <DateRangePicker value={range} onChange={setRange} />
      </div>

      <div className="grid gap-4 md:grid-cols-5">
        <StatCard label="Cost" value={formatCost(totals?.cost_usd ?? 0)} />
        <StatCard label="Tasks" value={(totals?.task_count ?? 0).toLocaleString()} />
        <StatCard label="Tokens in" value={formatTokens(totals?.input_tokens ?? 0)} />
        <StatCard label="Tokens out" value={formatTokens(totals?.output_tokens ?? 0)} />
        <StatCard label="Cache reads" value={formatTokens(totals?.cache_read_input_tokens ?? 0)} />
      </div>

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Token usage over time</CardTitle>
        </CardHeader>
        <CardContent>
          <TokenUsageChart data={stats.data?.history ?? []} gradientId="agent-usage" />
        </CardContent>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <CostBarChart title="Top models" rows={topModels} />
        <CostBarChart
          title="Top identities"
          rows={topIdentities}
          color="hsl(160, 60%, 45%)"
        />
      </div>

      {billing && billing.subscription_calls > 0 && (
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">Billing</CardTitle>
            <CardDescription>
              Every billable call on this agent — turns, vision, transcription and workflow steps — by
              what paid for it: a total of its own beside the task costs above.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="grid gap-4 md:grid-cols-3">
              <StatCard label="Metered spend" value={billedCost(billing.metered_cost_usd, "metered")} />
              <div title={ESTIMATE_TITLE}>
                <StatCard
                  label="Subscription value (est.)"
                  value={billedCost(billing.subscription_cost_usd, "subscription")}
                />
              </div>
              <StatCard label="Calls on a plan" value={billing.subscription_calls.toLocaleString()} />
            </div>
            <div className="grid gap-4 lg:grid-cols-2">
              <CostBarChart
                title="By origin"
                series={BILLING_SERIES}
                rows={(stats.data?.by_origin ?? []).map((r) => billingBarRow(originLabel(r.origin), r.split))}
              />
              <DataTable<PayerRow>
                columns={payerCols}
                rows={stats.data?.by_payer ?? []}
                rowKey={(r) => `${r.payer}:${r.credential_tier ?? ""}`}
                empty="No calls ran on a plan in range."
              />
            </div>
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Tools used</CardTitle>
        </CardHeader>
        <CardContent>
          <DataTable<ToolBreakdown>
            columns={toolCols}
            rows={stats.data?.tool_breakdown ?? []}
            rowKey={(r) => r.tool_name}
            empty="No tool calls in range."
          />
        </CardContent>
      </Card>

      {stats.data && stats.data.round_outliers.length > 0 && (
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">Most expensive tasks</CardTitle>
          </CardHeader>
          <CardContent>
            <DataTable<TaskHistoryEntry>
              columns={taskCols}
              rows={stats.data.round_outliers}
              rowKey={(r) => r.id}
              onRowClick={(r) => r.task_id && setTrace(r.task_id)}
            />
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Recent tasks</CardTitle>
        </CardHeader>
        <CardContent>
          <DataTable<TaskHistoryEntry>
            columns={taskCols}
            rows={tasks.data?.entries ?? []}
            rowKey={(r) => r.id}
            isLoading={tasks.isLoading}
            onRowClick={(r) => r.task_id && setTrace(r.task_id)}
            footer={
              tasks.data?.next_cursor && (
                <button
                  onClick={() => setCursor(tasks.data.next_cursor!)}
                  className="rounded-md px-3 py-1 hover:bg-muted"
                >
                  Load more →
                </button>
              )
            }
          />
        </CardContent>
      </Card>

      <TraceDrawer taskId={trace} onClose={() => setTrace(null)} />
    </div>
  );
}

