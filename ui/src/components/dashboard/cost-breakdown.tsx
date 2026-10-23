import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from "recharts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import type { CostSummary } from "@/lib/api/cost";

function toSortedEntries(record: Record<string, number> | null | undefined): { name: string; cost: number }[] {
  if (!record) return [];
  return Object.entries(record)
    .map(([name, cost]) => ({ name, cost }))
    .sort((a, b) => b.cost - a.cost);
}

function CostBar({ entries }: { entries: { name: string; cost: number }[] }) {
  if (entries.length === 0) {
    return (
      <p className="py-8 text-center text-sm text-muted-foreground">
        No cost data available.
      </p>
    );
  }

  return (
    <ResponsiveContainer width="100%" height={Math.max(180, entries.length * 40)}>
      <BarChart data={entries} layout="vertical" margin={{ left: 10, right: 20 }}>
        <CartesianGrid strokeDasharray="3 3" className="stroke-border" horizontal={false} />
        <XAxis
          type="number"
          tickFormatter={(v: number) => `$${v.toFixed(2)}`}
          tick={{ fontSize: 11 }}
          className="text-muted-foreground"
        />
        <YAxis
          type="category"
          dataKey="name"
          width={120}
          tick={{ fontSize: 11 }}
          className="text-muted-foreground"
        />
        <Tooltip
          formatter={(value: unknown) => [`$${Number(value ?? 0).toFixed(4)}`, "Cost"]}
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            border: "1px solid hsl(var(--border))",
            borderRadius: "0.5rem",
            fontSize: "0.75rem",
          }}
        />
        <Bar dataKey="cost" fill="hsl(221, 83%, 53%)" radius={[0, 4, 4, 0]} />
      </BarChart>
    </ResponsiveContainer>
  );
}

interface CostBreakdownProps {
  data: CostSummary | undefined;
  isLoading: boolean;
}

export function CostBreakdown({ data, isLoading }: CostBreakdownProps) {
  if (isLoading || !data) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Cost Breakdown</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-48 w-full" />
        </CardContent>
      </Card>
    );
  }

  const byAgent = toSortedEntries(data.by_agent);
  const byModel = toSortedEntries(data.by_model);

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Cost Breakdown</CardTitle>
        <p className="text-xs text-muted-foreground">
          Total: ${(data.total_cost_usd ?? 0).toFixed(4)} across {data.record_count ?? 0} records
        </p>
      </CardHeader>
      <CardContent>
        <Tabs defaultValue="agent">
          <TabsList>
            <TabsTrigger value="agent">By Agent</TabsTrigger>
            <TabsTrigger value="model">By Model</TabsTrigger>
          </TabsList>
          <TabsContent value="agent">
            <CostBar entries={byAgent} />
          </TabsContent>
          <TabsContent value="model">
            <CostBar entries={byModel} />
          </TabsContent>
        </Tabs>
      </CardContent>
    </Card>
  );
}
