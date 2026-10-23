import {
  Bar,
  BarChart,
  Cell,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Recall } from "@/lib/api/lessons";

const HIT_COLOR = "hsl(160, 60%, 45%)";
const MISS_COLOR = "hsl(38, 92%, 50%)";

interface Props {
  recall: Recall | null;
  isLoading?: boolean;
}

export function RecallCard({ recall, isLoading }: Props) {
  const hasData = recall != null && recall.hits + recall.misses > 0;

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Recall hit-rate</CardTitle>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            Loading…
          </div>
        ) : !hasData ? (
          <div className="text-sm text-muted-foreground p-6 text-center">
            No data yet
          </div>
        ) : (
          <div className="flex items-center gap-6">
            <div>
              <div className="text-3xl font-bold tabular-nums">
                {recall.rate != null
                  ? `${(recall.rate * 100).toFixed(0)}%`
                  : "—"}
              </div>
              <div className="text-xs text-muted-foreground mt-1">
                surfaced when re-taught
              </div>
              <div className="mt-3 flex gap-4 text-xs">
                <span className="text-success">{recall.hits} hits</span>
                <span className="text-warning">{recall.misses} missed</span>
              </div>
            </div>
            <div className="flex-1 min-w-0">
              <ResponsiveContainer width="100%" height={90}>
                <BarChart
                  data={[
                    { label: "Hits", value: recall.hits },
                    { label: "Missed", value: recall.misses },
                  ]}
                  layout="vertical"
                  margin={{ left: 4, right: 8 }}
                >
                  {/* Vertical layout needs an explicit number (X) + category (Y)
                      axis or recharts silently renders nothing; hide both. */}
                  <XAxis type="number" hide />
                  <YAxis type="category" dataKey="label" hide />
                  <Tooltip
                    cursor={{ fill: "transparent" }}
                    formatter={(v: unknown) => [String(v ?? 0), "Count"]}
                    contentStyle={{
                      backgroundColor: "var(--color-card)",
                      border: "1px solid var(--color-border)",
                      borderRadius: "0.5rem",
                      fontSize: "0.75rem",
                    }}
                  />
                  <Bar dataKey="value" radius={[0, 3, 3, 0]} barSize={22}>
                    <Cell fill={HIT_COLOR} />
                    <Cell fill={MISS_COLOR} />
                  </Bar>
                </BarChart>
              </ResponsiveContainer>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
