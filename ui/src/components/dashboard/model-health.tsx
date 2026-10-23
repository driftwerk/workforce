import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Badge } from "@/components/ui/badge";
import { ChevronRight } from "lucide-react";

interface ModelEntry {
  name: string;
  status: string;
  failures: number;
  degraded: boolean;
}

function parseModels(
  healthData: { enabled: boolean; models: Record<string, unknown> } | undefined,
): ModelEntry[] {
  if (!healthData?.models) return [];
  return Object.entries(healthData.models).map(([name, raw]) => {
    const m = raw as Record<string, unknown> | null;
    return {
      name,
      status: typeof m?.["status"] === "string" ? m["status"] : "unknown",
      failures: typeof m?.["recent_failures"] === "number" ? m["recent_failures"] : 0,
      degraded: typeof m?.["degraded"] === "boolean" ? m["degraded"] : false,
    };
  });
}

function modelBadgeVariant(
  entry: ModelEntry,
): "success" | "warning" | "destructive" | "secondary" {
  if (entry.degraded || entry.status === "degraded") return "warning";
  if (entry.status === "down" || entry.status === "error") return "destructive";
  if (entry.status === "ok" || entry.status === "healthy") return "success";
  return "secondary";
}

interface ModelHealthProps {
  healthData: { enabled: boolean; models: Record<string, unknown> } | undefined;
  aliasData:
    | {
        default_model: string;
        failover_chain: string[];
        aliases: Record<string, string>;
      }
    | undefined;
  isLoading: boolean;
}

export function ModelHealth({ healthData, aliasData, isLoading }: ModelHealthProps) {
  if (isLoading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Model Health</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-36 w-full" />
        </CardContent>
      </Card>
    );
  }

  const models = parseModels(healthData);
  const chain = aliasData?.failover_chain ?? [];

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Model Health</CardTitle>
        {aliasData?.default_model && (
          <p className="text-xs text-muted-foreground">
            Default: {aliasData.default_model}
          </p>
        )}
      </CardHeader>
      <CardContent className="space-y-4">
        {models.length === 0 && (
          <p className="py-4 text-center text-sm text-muted-foreground">
            No model data available.
          </p>
        )}

        {models.length > 0 && (
          <div className="grid gap-2">
            {models.map((m) => (
              <div
                key={m.name}
                className="flex items-center justify-between rounded-md border px-3 py-2"
              >
                <div className="flex flex-col gap-0.5">
                  <span className="text-sm font-medium">{m.name}</span>
                  {m.failures > 0 && (
                    <span className="text-xs text-muted-foreground">
                      {m.failures} recent failure{m.failures !== 1 ? "s" : ""}
                    </span>
                  )}
                </div>
                <Badge variant={modelBadgeVariant(m)}>
                  {m.degraded ? "degraded" : m.status}
                </Badge>
              </div>
            ))}
          </div>
        )}

        {chain.length > 0 && (
          <div>
            <p className="mb-1.5 text-xs font-medium text-muted-foreground">
              Failover Chain
            </p>
            <div className="flex flex-wrap items-center gap-1 text-xs">
              {chain.map((model, i) => (
                <span key={model} className="flex items-center gap-1">
                  <span className="rounded bg-muted px-2 py-0.5 font-mono">
                    {model}
                  </span>
                  {i < chain.length - 1 && (
                    <ChevronRight className="h-3 w-3 text-muted-foreground" />
                  )}
                </span>
              ))}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
