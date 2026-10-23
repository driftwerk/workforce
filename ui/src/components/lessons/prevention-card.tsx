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
import type { Prevention } from "@/lib/api/lessons";

const PREVENTED_COLOR = "hsl(160, 60%, 45%)";
const FAILED_COLOR = "hsl(0, 72%, 51%)";

interface Props {
  prevention: Prevention | null;
  isLoading?: boolean;
}

export function PreventionCard({ prevention, isLoading }: Props) {
  const hasData =
    prevention != null && prevention.prevented + prevention.failed > 0;

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Prevention</CardTitle>
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
                {prevention.rate != null
                  ? `${(prevention.rate * 100).toFixed(0)}%`
                  : "—"}
              </div>
              <div className="text-xs text-muted-foreground mt-1">
                prevention rate
              </div>
              <div className="mt-3 flex gap-4 text-xs">
                <span className="text-success">
                  {prevention.prevented} prevented
                </span>
                <span className="text-destructive">
                  {prevention.failed} failed
                </span>
              </div>
            </div>
            <div className="flex-1 min-w-0">
              <ResponsiveContainer width="100%" height={90}>
                <BarChart
                  data={[
                    { label: "Prevented", value: prevention.prevented },
                    { label: "Failed", value: prevention.failed },
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
                    <Cell fill={PREVENTED_COLOR} />
                    <Cell fill={FAILED_COLOR} />
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
