import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { formatDuration, formatCost } from "@/lib/utils";
import type { InstanceCost } from "@/lib/api/workflows";
import { DollarSign, Timer, Layers, Zap } from "lucide-react";

interface InstanceCostSummaryProps {
  cost: InstanceCost;
}

export function InstanceCostSummary({ cost }: InstanceCostSummaryProps) {
  return (
    <div className="space-y-4">
      {/* Summary cards */}
      <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
        <Card>
          <CardHeader className="p-3 pb-1">
            <CardTitle className="text-xs text-muted-foreground flex items-center gap-1.5">
              <DollarSign className="h-3 w-3" />
              Total Cost
            </CardTitle>
          </CardHeader>
          <CardContent className="p-3 pt-0">
            <div className="text-lg font-semibold">{formatCost(cost.total_cost)}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="p-3 pb-1">
            <CardTitle className="text-xs text-muted-foreground flex items-center gap-1.5">
              <Layers className="h-3 w-3" />
              Rounds
            </CardTitle>
          </CardHeader>
          <CardContent className="p-3 pt-0">
            <div className="text-lg font-semibold">{cost.total_rounds}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="p-3 pb-1">
            <CardTitle className="text-xs text-muted-foreground flex items-center gap-1.5">
              <Timer className="h-3 w-3" />
              Duration
            </CardTitle>
          </CardHeader>
          <CardContent className="p-3 pt-0">
            <div className="text-lg font-semibold">{formatDuration(cost.total_duration_ms)}</div>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="p-3 pb-1">
            <CardTitle className="text-xs text-muted-foreground flex items-center gap-1.5">
              <Zap className="h-3 w-3" />
              Tokens
            </CardTitle>
          </CardHeader>
          <CardContent className="p-3 pt-0">
            <div className="text-lg font-semibold">{cost.total_tokens.toLocaleString()}</div>
          </CardContent>
        </Card>
      </div>

      {/* Per-step breakdown */}
      {cost.steps.length > 0 && (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full text-xs">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className="text-left font-medium text-muted-foreground px-3 py-2">Step</th>
                <th className="text-right font-medium text-muted-foreground px-3 py-2">Cost</th>
                <th className="text-right font-medium text-muted-foreground px-3 py-2">Tokens</th>
                <th className="text-right font-medium text-muted-foreground px-3 py-2">Rounds</th>
                <th className="text-right font-medium text-muted-foreground px-3 py-2">Duration</th>
              </tr>
            </thead>
            <tbody>
              {cost.steps.map((s) => (
                <tr key={s.step_id} className="border-b last:border-b-0">
                  <td className="px-3 py-2 font-mono">{s.step_id}</td>
                  <td className="px-3 py-2 text-right">{formatCost(s.cost)}</td>
                  <td className="px-3 py-2 text-right">{s.tokens.toLocaleString()}</td>
                  <td className="px-3 py-2 text-right">{s.rounds}</td>
                  <td className="px-3 py-2 text-right">{formatDuration(s.duration_ms)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
