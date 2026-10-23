import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Bot,
  ListTodo,
  Users,
  DollarSign,
  MonitorDot,
  type LucideIcon,
} from "lucide-react";
import type { DashboardView } from "@/lib/api/dashboard";

interface FeedItem {
  id: string;
  icon: LucideIcon;
  description: string;
  timestamp: string;
}

function buildFeed(data: DashboardView): FeedItem[] {
  const items: FeedItem[] = [];
  if (!data.agents || !data.tasks || !data.teams || !data.nodes || !data.cost)
    return items;
  const now = new Date();

  if (data.agents.active > 0) {
    items.push({
      id: "agents-active",
      icon: Bot,
      description: `${data.agents.active} agent${data.agents.active !== 1 ? "s" : ""} currently active`,
      timestamp: formatRelative(now),
    });
  }

  if (data.tasks.in_progress > 0) {
    items.push({
      id: "tasks-progress",
      icon: ListTodo,
      description: `${data.tasks.in_progress} task${data.tasks.in_progress !== 1 ? "s" : ""} in progress`,
      timestamp: formatRelative(now),
    });
  }

  if (data.tasks.completed > 0) {
    items.push({
      id: "tasks-done",
      icon: ListTodo,
      description: `${data.tasks.completed} task${data.tasks.completed !== 1 ? "s" : ""} completed`,
      timestamp: formatRelative(now),
    });
  }

  if (data.tasks.failed > 0) {
    items.push({
      id: "tasks-failed",
      icon: ListTodo,
      description: `${data.tasks.failed} task${data.tasks.failed !== 1 ? "s" : ""} failed`,
      timestamp: formatRelative(now),
    });
  }

  if (data.teams.total > 0) {
    items.push({
      id: "teams",
      icon: Users,
      description: `${data.teams.total} team${data.teams.total !== 1 ? "s" : ""} configured with ${data.teams.total_members} member${data.teams.total_members !== 1 ? "s" : ""}`,
      timestamp: formatRelative(now),
    });
  }

  if (data.nodes.online > 0) {
    items.push({
      id: "nodes-online",
      icon: MonitorDot,
      description: `${data.nodes.online} of ${data.nodes.total} node${data.nodes.total !== 1 ? "s" : ""} online`,
      timestamp: formatRelative(now),
    });
  }

  if (data.cost.top_agent) {
    items.push({
      id: "top-agent",
      icon: DollarSign,
      description: `Top spending agent: ${data.cost.top_agent}`,
      timestamp: formatRelative(now),
    });
  }

  if (data.cost.top_model) {
    items.push({
      id: "top-model",
      icon: DollarSign,
      description: `Most used model: ${data.cost.top_model}`,
      timestamp: formatRelative(now),
    });
  }

  if (data.agents.idle > 0) {
    items.push({
      id: "agents-idle",
      icon: Bot,
      description: `${data.agents.idle} agent${data.agents.idle !== 1 ? "s" : ""} idle`,
      timestamp: formatRelative(now),
    });
  }

  if (data.nodes.offline > 0) {
    items.push({
      id: "nodes-offline",
      icon: MonitorDot,
      description: `${data.nodes.offline} node${data.nodes.offline !== 1 ? "s" : ""} offline`,
      timestamp: formatRelative(now),
    });
  }

  return items;
}

function formatRelative(_date: Date): string {
  return "just now";
}

interface ActivityFeedProps {
  data: DashboardView | undefined;
  isLoading: boolean;
}

export function ActivityFeed({ data, isLoading }: ActivityFeedProps) {
  if (isLoading || !data) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Recent Activity</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-48 w-full" />
        </CardContent>
      </Card>
    );
  }

  const items = buildFeed(data);

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Recent Activity</CardTitle>
      </CardHeader>
      <CardContent>
        {items.length === 0 ? (
          <p className="py-6 text-center text-sm text-muted-foreground">
            No activity to display.
          </p>
        ) : (
          <div className="space-y-0">
            {items.map((item, i) => {
              const Icon = item.icon;
              return (
                <div key={item.id} className="flex items-start gap-3 py-2.5">
                  <div className="relative flex flex-col items-center">
                    <div className="flex h-7 w-7 items-center justify-center rounded-full bg-muted">
                      <Icon className="h-3.5 w-3.5 text-muted-foreground" />
                    </div>
                    {i < items.length - 1 && (
                      <div className="mt-1 h-full w-px bg-border" />
                    )}
                  </div>
                  <div className="flex-1 pt-0.5">
                    <p className="text-sm">{item.description}</p>
                    <p className="text-xs text-muted-foreground">
                      {item.timestamp}
                    </p>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
