import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Badge } from "@/components/ui/badge";

type ServiceStatus = "healthy" | "degraded" | "down" | "unknown";

interface ServiceEntry {
  name: string;
  status: ServiceStatus;
}

function statusColor(status: ServiceStatus): string {
  switch (status) {
    case "healthy":
      return "bg-emerald-500";
    case "degraded":
      return "bg-amber-500";
    case "down":
      return "bg-red-500";
    default:
      return "bg-zinc-400";
  }
}

function statusBadgeVariant(
  status: ServiceStatus,
): "success" | "warning" | "destructive" | "secondary" {
  switch (status) {
    case "healthy":
      return "success";
    case "degraded":
      return "warning";
    case "down":
      return "destructive";
    default:
      return "secondary";
  }
}

function deriveServices(
  health: { status: string; version: string } | undefined,
): ServiceEntry[] {
  // The health endpoint returns a simple status/version response.
  // We model the core services with the overall health as their proxy.
  const overall: ServiceStatus =
    health?.status === "ok" || health?.status === "healthy"
      ? "healthy"
      : health?.status === "degraded"
        ? "degraded"
        : health
          ? "down"
          : "unknown";

  return [
    { name: "Gateway", status: overall },
    { name: "Agent Service", status: overall },
    { name: "Scheduler", status: overall },
    { name: "NATS", status: overall },
    { name: "Slack", status: overall },
  ];
}

interface HealthPanelProps {
  data: { status: string; version: string } | undefined;
  isLoading: boolean;
}

export function HealthPanel({ data, isLoading }: HealthPanelProps) {
  if (isLoading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Service Health</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-36 w-full" />
        </CardContent>
      </Card>
    );
  }

  const services = deriveServices(data);

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-base">Service Health</CardTitle>
        {data?.version && (
          <span className="text-xs text-muted-foreground">v{data.version}</span>
        )}
      </CardHeader>
      <CardContent>
        <div className="grid gap-3">
          {services.map((svc) => (
            <div
              key={svc.name}
              className="flex items-center justify-between rounded-md border px-3 py-2"
            >
              <div className="flex items-center gap-2">
                <span
                  className={`inline-block h-2.5 w-2.5 rounded-full ${statusColor(svc.status)}`}
                />
                <span className="text-sm font-medium">{svc.name}</span>
              </div>
              <Badge variant={statusBadgeVariant(svc.status)}>
                {svc.status}
              </Badge>
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}
