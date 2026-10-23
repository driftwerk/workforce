import {
  Area,
  AreaChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import type { TokenHistoryPoint } from "@/lib/api/cost";

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

const INPUT_COLOR = "hsl(221, 83%, 53%)";
const OUTPUT_COLOR = "hsl(160, 60%, 45%)";

interface Props {
  data: TokenHistoryPoint[];
  /// Used to namespace recharts gradient `<linearGradient id>`s when
  /// multiple TokenUsageCharts render on the same page.
  gradientId: string;
  height?: number;
  emptyLabel?: string;
}

export function TokenUsageChart({
  data,
  gradientId,
  height = 260,
  emptyLabel = "No usage in range.",
}: Props) {
  if (data.length === 0) {
    return <div className="text-sm text-muted-foreground p-6">{emptyLabel}</div>;
  }
  const points = data.map((p) => ({
    date: new Date(p.bucket_start).toLocaleDateString("en-US", {
      month: "short",
      day: "numeric",
      timeZone: "UTC",
    }),
    input: p.input_tokens,
    output: p.output_tokens,
  }));
  const inputId = `${gradientId}-input`;
  const outputId = `${gradientId}-output`;
  return (
    <ResponsiveContainer width="100%" height={height}>
      <AreaChart data={points}>
        <defs>
          <linearGradient id={inputId} x1="0" y1="0" x2="0" y2="1">
            <stop offset="5%" stopColor={INPUT_COLOR} stopOpacity={0.3} />
            <stop offset="95%" stopColor={INPUT_COLOR} stopOpacity={0} />
          </linearGradient>
          <linearGradient id={outputId} x1="0" y1="0" x2="0" y2="1">
            <stop offset="5%" stopColor={OUTPUT_COLOR} stopOpacity={0.3} />
            <stop offset="95%" stopColor={OUTPUT_COLOR} stopOpacity={0} />
          </linearGradient>
        </defs>
        <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
        <XAxis dataKey="date" tick={{ fontSize: 11 }} />
        <YAxis tickFormatter={formatTokens} tick={{ fontSize: 11 }} />
        <Tooltip
          formatter={(v: unknown, name: unknown) => [
            formatTokens(Number(v ?? 0)),
            name === "input" ? "Input" : "Output",
          ]}
          contentStyle={{
            backgroundColor: "hsl(var(--card))",
            border: "1px solid hsl(var(--border))",
            borderRadius: "0.5rem",
            fontSize: "0.75rem",
          }}
        />
        <Area
          type="monotone"
          dataKey="input"
          stroke={INPUT_COLOR}
          fill={`url(#${inputId})`}
          strokeWidth={2}
        />
        <Area
          type="monotone"
          dataKey="output"
          stroke={OUTPUT_COLOR}
          fill={`url(#${outputId})`}
          strokeWidth={2}
        />
      </AreaChart>
    </ResponsiveContainer>
  );
}
