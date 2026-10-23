import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Play, Loader2 } from "lucide-react";
import { schedulerApi, type ScheduledJob, type JobSchedule } from "@/lib/api/scheduler";

interface JobTableProps {
  jobs: ScheduledJob[];
  onTriggered?: () => void;
}

function formatSchedule(schedule: JobSchedule): string {
  switch (schedule.kind) {
    case "at":
      return `Once at ${new Date(schedule.at).toLocaleString()}`;
    case "every": {
      const secs = schedule.every_secs;
      if (secs < 60) return `Every ${secs}s`;
      if (secs < 3600) return `Every ${Math.round(secs / 60)}m`;
      if (secs < 86400) return `Every ${Math.round(secs / 3600)}h`;
      return `Every ${Math.round(secs / 86400)}d`;
    }
    case "cron":
      return schedule.expr + (schedule.tz ? ` (${schedule.tz})` : "");
    default:
      return "Unknown";
  }
}

function formatDate(iso: string | null): string {
  if (!iso) return "--";
  return new Date(iso).toLocaleString();
}

export function JobTable({ jobs, onTriggered }: JobTableProps) {
  const [triggeringId, setTriggeringId] = useState<string | null>(null);

  const handleTrigger = async (id: string) => {
    setTriggeringId(id);
    try {
      await schedulerApi.trigger(id);
      onTriggered?.();
    } catch {
      // Error is silently handled; the user sees the button reset
    } finally {
      setTriggeringId(null);
    }
  };

  return (
    <div className="rounded-md border overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b bg-muted/50">
            <th className="text-left font-medium px-4 py-3">Name</th>
            <th className="text-left font-medium px-4 py-3">Schedule</th>
            <th className="text-left font-medium px-4 py-3">Status</th>
            <th className="text-left font-medium px-4 py-3">Last Run</th>
            <th className="text-left font-medium px-4 py-3">Next Run</th>
            <th className="text-left font-medium px-4 py-3">Runs</th>
            <th className="text-right font-medium px-4 py-3">Actions</th>
          </tr>
        </thead>
        <tbody>
          {jobs.map((job) => (
            <tr key={job.id} className="border-b last:border-b-0 hover:bg-muted/30 transition-colors">
              <td className="px-4 py-3">
                <div>
                  <span className="font-medium">{job.name}</span>
                  {job.description && (
                    <p className="text-xs text-muted-foreground mt-0.5 line-clamp-1">
                      {job.description}
                    </p>
                  )}
                </div>
              </td>
              <td className="px-4 py-3 font-mono text-xs">
                {formatSchedule(job.schedule)}
              </td>
              <td className="px-4 py-3">
                <Badge variant={job.enabled ? "success" : "secondary"}>
                  {job.enabled ? "Active" : "Disabled"}
                </Badge>
              </td>
              <td className="px-4 py-3 text-xs text-muted-foreground whitespace-nowrap">
                {formatDate(job.last_run)}
              </td>
              <td className="px-4 py-3 text-xs text-muted-foreground whitespace-nowrap">
                {formatDate(job.next_run)}
              </td>
              <td className="px-4 py-3 text-xs text-muted-foreground">
                {job.run_count}
              </td>
              <td className="px-4 py-3 text-right">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={triggeringId === job.id}
                  onClick={() => handleTrigger(job.id)}
                >
                  {triggeringId === job.id ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <Play className="h-3.5 w-3.5" />
                  )}
                  <span className="ml-1.5">Trigger</span>
                </Button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
