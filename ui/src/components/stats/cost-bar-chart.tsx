import {
  Bar,
  BarChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { formatCost } from "@/lib/utils";

export interface CostBarRow {
  /// Display label rendered on the y-axis.
  label: string;
  /// Cost in USD.
  cost: number;
  /// Cost in USD per series key, for a chart stacked by `series`.
  parts?: Record<string, number>;
  /// Original row passed back to `onClick` so callers can navigate.
  raw?: unknown;
}

export interface CostSeries {
  key: string;
  label: string;
  color: string;
}

interface Props {
  title: string;
  rows: CostBarRow[];
  onRowClick?: (row: CostBarRow) => void;
  color?: string;
  /// Stack each bar from `parts` instead of drawing `cost` alone.
  series?: readonly CostSeries[];
  emptyLabel?: string;
}

export function CostBarChart({
  title,
  rows,
  onRowClick,
  color = "hsl(221, 83%, 53%)",
  series,
  emptyLabel = "No data in range.",
}: Props) {
  const bars: readonly CostSeries[] = series ?? [{ key: "cost", label: "Cost", color }];
  const value = (key: string) => (row: CostBarRow) =>
    series ? (row.parts?.[key] ?? 0) : row.cost;
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">{title}</CardTitle>
      </CardHeader>
      <CardContent>
        {rows.length === 0 ? (
          <div className="text-sm text-muted-foreground p-6">{emptyLabel}</div>
        ) : (
          <ResponsiveContainer width="100%" height={240}>
            <BarChart data={rows} layout="vertical" margin={{ left: 60 }}>
              <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
              <XAxis
                type="number"
                tickFormatter={(v) => formatCost(Number(v))}
                tick={{ fontSize: 11 }}
              />
              <YAxis
                type="category"
                dataKey="label"
                tick={{ fontSize: 11 }}
                width={120}
              />
              <Tooltip
                formatter={(v: unknown) => formatCost(Number(v ?? 0))}
                contentStyle={{
                  backgroundColor: "hsl(var(--card))",
                  border: "1px solid hsl(var(--border))",
                  borderRadius: "0.5rem",
                  fontSize: "0.75rem",
                }}
              />
              {series && <Legend wrapperStyle={{ fontSize: "0.75rem" }} />}
              {bars.map((bar) => (
                <Bar
                  key={bar.key}
                  name={bar.label}
                  dataKey={value(bar.key)}
                  stackId="cost"
                  fill={bar.color}
                  onClick={
                    onRowClick
                      ? (d) => onRowClick(d as unknown as CostBarRow)
                      : undefined
                  }
                  style={onRowClick ? { cursor: "pointer" } : undefined}
                />
              ))}
            </BarChart>
          </ResponsiveContainer>
        )}
      </CardContent>
    </Card>
  );
}
