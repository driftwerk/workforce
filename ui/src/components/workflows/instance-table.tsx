import { Button } from "@/components/ui/button";
import { InstanceStatusBadge } from "@/components/workflows/instance-status-badge";
import { useResumeWorkflow, useAbortWorkflow } from "@/hooks/use-workflows";
import { formatDateTime, formatRelativeTime } from "@/lib/utils";
import type { WorkflowInstance } from "@/lib/api/workflows";
import { apiErrorMessage } from "@/lib/api-error";

interface InstanceTableProps {
  instances: WorkflowInstance[];
  onStatusFilter?: (status: string | undefined) => void;
  activeStatus?: string;
  showWorkflowId?: boolean;
  onSelectInstance?: (instanceId: string) => void;
}

const STATUS_FILTERS = ["all", "running", "paused", "completed", "failed", "cancelled"] as const;

export function InstanceTable({
  instances,
  onStatusFilter,
  activeStatus,
  showWorkflowId = false,
  onSelectInstance,
}: InstanceTableProps) {
  const resumeMutation = useResumeWorkflow();
  const abortMutation = useAbortWorkflow();

  const isActive = (inst: WorkflowInstance) =>
    inst.status === "running" || inst.status === "paused";
  const refusal = resumeMutation.error ?? abortMutation.error;

  return (
    <div className="space-y-3">
      {/* Status filter pills */}
      {onStatusFilter && (
        <div className="flex flex-wrap gap-1.5">
          {STATUS_FILTERS.map((s) => (
            <Button
              key={s}
              variant={
                (s === "all" ? !activeStatus : activeStatus === s)
                  ? "secondary"
                  : "outline"
              }
              size="sm"
              className="h-6 px-2.5 text-xs capitalize"
              onClick={() => onStatusFilter(s === "all" ? undefined : s)}
            >
              {s === "all" ? "All" : s.charAt(0).toUpperCase() + s.slice(1)}
            </Button>
          ))}
        </div>
      )}

      {refusal && <p className="text-xs text-destructive">{apiErrorMessage(refusal)}</p>}

      {instances.length === 0 ? (
        <div className="flex items-center justify-center py-12 text-sm text-muted-foreground">
          No instances found
        </div>
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b bg-muted/50">
                {showWorkflowId && (
                  <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                    Workflow
                  </th>
                )}
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                  Instance ID
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                  Status
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 hidden md:table-cell">
                  Current Step
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 hidden sm:table-cell">
                  Started
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-32">
                  Actions
                </th>
              </tr>
            </thead>
            <tbody>
              {instances.map((inst) => (
                <tr
                  key={inst.instance_id}
                  className={`border-b last:border-b-0 hover:bg-muted/20 transition-colors ${onSelectInstance ? "cursor-pointer" : ""}`}
                  onClick={() => onSelectInstance?.(inst.instance_id)}
                >
                  {showWorkflowId && (
                    <td className="px-4 py-3">
                      <span className="text-sm font-mono text-muted-foreground">
                        {inst.workflow_id}
                      </span>
                    </td>
                  )}
                  <td className="px-4 py-3">
                    <span className="text-xs font-mono text-muted-foreground truncate max-w-[140px] block">
                      {inst.instance_id}
                    </span>
                  </td>
                  <td className="px-4 py-3">
                    <InstanceStatusBadge status={inst.status} />
                  </td>
                  <td className="px-4 py-3 hidden md:table-cell">
                    <span className="text-sm text-muted-foreground font-mono">
                      {inst.current_step ?? "—"}
                    </span>
                  </td>
                  <td className="px-4 py-3 hidden sm:table-cell">
                    <div className="text-xs">{formatDateTime(inst.created_at)}</div>
                    <div className="text-[10px] text-muted-foreground">{formatRelativeTime(inst.created_at)}</div>
                  </td>
                  <td className="px-4 py-3 text-right">
                    <div className="flex items-center justify-end gap-1">
                      {inst.status === "paused" && !inst.awaiting_decision && (
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-6 px-2 text-xs"
                          disabled={resumeMutation.isPending}
                          onClick={() => resumeMutation.mutate({ id: inst.instance_id })}
                        >
                          Resume
                        </Button>
                      )}
                      {isActive(inst) && (
                        <Button
                          variant="ghost"
                          size="sm"
                          className="h-6 px-2 text-xs text-destructive hover:text-destructive"
                          disabled={abortMutation.isPending}
                          onClick={() => abortMutation.mutate(inst.instance_id)}
                        >
                          Abort
                        </Button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
