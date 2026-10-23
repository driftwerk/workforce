import { useDashboard, useHealth } from "@/hooks/use-dashboard";
import { useCostSummary } from "@/hooks/use-cost";
import { useModelHealth, useModelAliases } from "@/hooks/use-models";
import { PageSkeleton } from "@/components/loading-skeleton";
import { StatsCards } from "@/components/dashboard/stats-cards";
import { TokenChart } from "@/components/dashboard/token-chart";
import { CostBreakdown } from "@/components/dashboard/cost-breakdown";
import { HealthPanel } from "@/components/dashboard/health-panel";
import { ModelHealth } from "@/components/dashboard/model-health";
import { ActivityFeed } from "@/components/dashboard/activity-feed";

export default function DashboardPage() {
  const dashboard = useDashboard();
  const health = useHealth();
  const cost = useCostSummary();
  const modelHealth = useModelHealth();
  const modelAliases = useModelAliases();

  if (dashboard.isLoading && !dashboard.data) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-2xl font-bold">Dashboard</h1>
        <p className="text-muted-foreground">System overview at a glance.</p>
      </div>

      {/* Stats cards */}
      <StatsCards data={dashboard.data} />

      {/* Charts row */}
      <div className="grid gap-6 lg:grid-cols-2">
        <TokenChart />
        <CostBreakdown data={cost.data} isLoading={cost.isLoading} />
      </div>

      {/* Health + Models + Activity row */}
      <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-3">
        <HealthPanel data={health.data} isLoading={health.isLoading} />
        <ModelHealth
          healthData={modelHealth.data}
          aliasData={modelAliases.data}
          isLoading={modelHealth.isLoading}
        />
        <ActivityFeed data={dashboard.data} isLoading={dashboard.isLoading} />
      </div>
    </div>
  );
}
