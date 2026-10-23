import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Bot,
  Users,
  ListTodo,
  DollarSign,
  MonitorDot,
  TrendingUp,
  TrendingDown,
} from "lucide-react";
import type { DashboardView } from "@/lib/api/dashboard";

interface StatCardProps {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  value: string | number;
  sub: string;
  trend?: "up" | "down" | null;
}

function StatCard({ icon: Icon, label, value, sub, trend }: StatCardProps) {
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">{label}</CardTitle>
        <Icon className="h-4 w-4 text-muted-foreground" />
      </CardHeader>
      <CardContent>
        <div className="flex items-center gap-2">
          <div className="text-2xl font-bold">{value}</div>
          {trend === "up" && (
            <TrendingUp className="h-4 w-4 text-emerald-500" />
          )}
          {trend === "down" && (
            <TrendingDown className="h-4 w-4 text-red-500" />
          )}
        </div>
        <p className="text-xs text-muted-foreground">{sub}</p>
      </CardContent>
    </Card>
  );
}

export function StatsCardsSkeleton() {
  return (
    <div className="grid gap-4 grid-cols-2 lg:grid-cols-5">
      {Array.from({ length: 5 }).map((_, i) => (
        <Skeleton key={i} className="h-32 rounded-lg" />
      ))}
    </div>
  );
}

interface StatsCardsProps {
  data: DashboardView | undefined;
}

export function StatsCards({ data }: StatsCardsProps) {
  if (!data || !data.agents || !data.tasks || !data.teams || !data.nodes || !data.cost)
    return <StatsCardsSkeleton />;

  const d = data;
  return (
    <div className="grid gap-4 grid-cols-2 lg:grid-cols-5">
      <StatCard
        icon={Bot}
        label="Active Agents"
        value={d.agents.active}
        sub={`${d.agents.total} total, ${d.agents.idle} idle`}
        trend={d.agents.active > 0 ? "up" : null}
      />
      <StatCard
        icon={Users}
        label="Teams"
        value={d.teams.total}
        sub={`${d.teams.total_members} members`}
      />
      <StatCard
        icon={ListTodo}
        label="Tasks"
        value={d.tasks.total}
        sub={`${d.tasks.in_progress} in progress, ${d.tasks.completed} done`}
        trend={d.tasks.in_progress > 0 ? "up" : null}
      />
      <StatCard
        icon={DollarSign}
        label="Cost Today"
        value={`$${(d.cost.today_usd ?? 0).toFixed(2)}`}
        sub={`$${(d.cost.month_usd ?? 0).toFixed(2)} this month`}
      />
      <StatCard
        icon={MonitorDot}
        label="Nodes Online"
        value={d.nodes.online}
        sub={`${d.nodes.total} total, ${d.nodes.offline} offline`}
        trend={d.nodes.offline > 0 ? "down" : "up"}
      />
    </div>
  );
}
