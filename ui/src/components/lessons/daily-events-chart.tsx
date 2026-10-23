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
import type { TimeseriesPoint } from "@/lib/api/lessons";

interface SeriesDef {
  key: keyof Omit<TimeseriesPoint, "day">;
  label: string;
  color: string;
}

// Stacked event series — one colour per lesson-pipeline event type.
const SERIES: SeriesDef[] = [
  { key: "injected", label: "Injected", color: "hsl(221, 83%, 53%)" },
  { key: "suppressed", label: "Suppressed", color: "hsl(221, 30%, 65%)" },
  { key: "reinforced_positive", label: "Reinforced +", color: "hsl(160, 60%, 45%)" },
  { key: "reinforced_negative", label: "Reinforced −", color: "hsl(0, 72%, 51%)" },
  { key: "extracted", label: "Extracted", color: "hsl(43, 96%, 56%)" },
  { key: "merged_recurrence", label: "Merged recurrence", color: "hsl(280, 60%, 60%)" },
  { key: "hit", label: "Recall hit", color: "hsl(190, 70%, 45%)" },
  { key: "recall_miss", label: "Recall miss", color: "hsl(20, 75%, 42%)" },
  { key: "suppressed_repeat", label: "Suppressed repeat", color: "hsl(20, 40%, 60%)" },
  { key: "superseded", label: "Superseded", color: "hsl(330, 65%, 55%)" },
  { key: "pruned", label: "Pruned", color: "hsl(0, 0%, 55%)" },
];

function shortDay(day: string): string {
  const d = new Date(`${day}T00:00:00Z`);
  if (Number.isNaN(d.getTime())) return day;
  return d.toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  });
}

export function DailyEventsChart({
  data,
  isLoading,
}: {
  data: TimeseriesPoint[];
  isLoading?: boolean;
}) {
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Daily events</CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            Loading…
          </div>
        ) : data.length === 0 ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            No events in range.
          </div>
        ) : (
          <ResponsiveContainer width="100%" height={260}>
            <BarChart data={data} margin={{ top: 4, right: 8, left: 0, bottom: 4 }}>
              <CartesianGrid strokeDasharray="3 3" className="stroke-border" />
              <XAxis
                dataKey="day"
                tick={{ fontSize: 11 }}
                tickFormatter={shortDay}
              />
              <YAxis tick={{ fontSize: 11 }} allowDecimals={false} width={32} />
              <Tooltip
                labelFormatter={(label) => shortDay(String(label))}
                contentStyle={{
                  backgroundColor: "var(--color-card)",
                  border: "1px solid var(--color-border)",
                  borderRadius: "0.5rem",
                  fontSize: "0.75rem",
                }}
              />
              <Legend wrapperStyle={{ fontSize: "0.7rem" }} />
              {SERIES.map((s) => (
                <Bar
                  key={s.key}
                  dataKey={s.key}
                  name={s.label}
                  stackId="events"
                  fill={s.color}
                />
              ))}
            </BarChart>
          </ResponsiveContainer>
        )}
      </CardContent>
    </Card>
  );
}
