import { StatCard } from "@/components/stats/stat-card";
import { formatCostScaled } from "@/lib/utils";
import type { LessonsSummary } from "@/lib/api/lessons";

interface Props {
  summary: LessonsSummary | undefined;
  isLoading?: boolean;
}

export function SummaryCards({ summary, isLoading }: Props) {
  const total = summary?.total ?? 0;
  const appliedPct =
    total > 0 ? Math.round(((summary?.applied_at_least_once ?? 0) / total) * 100) : 0;

  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6">
      <StatCard label="Total lessons" value={total.toLocaleString()} isLoading={isLoading} />
      <StatCard
        label={`Applied ≥1 (${appliedPct}%)`}
        value={(summary?.applied_at_least_once ?? 0).toLocaleString()}
        isLoading={isLoading}
      />
      <StatCard
        label="Never applied"
        value={(summary?.never_applied ?? 0).toLocaleString()}
        isLoading={isLoading}
      />
      <StatCard
        label="Avg confidence"
        value={(summary?.avg_confidence ?? 0).toFixed(2)}
        isLoading={isLoading}
      />
      <StatCard
        label="Open recurrences"
        value={(summary?.open_recurrences ?? 0).toLocaleString()}
        isLoading={isLoading}
      />
      <StatCard
        label={`Extraction cost (${summary?.window_days ?? 0}d)`}
        value={formatCostScaled(summary?.extraction_cost_usd ?? 0)}
        isLoading={isLoading}
      />
    </div>
  );
}
