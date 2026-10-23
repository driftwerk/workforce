import { Badge } from "@/components/ui/badge";
import { formatCost, formatDateTime, formatDuration, formatRelativeTime } from "@/lib/utils";
import type { TimelineStep } from "@/lib/api/workflows";
import { CheckCircle2, XCircle, Pause, SkipForward, Clock, Circle } from "lucide-react";

type StatusVariant = "default" | "secondary" | "destructive" | "outline" | "success" | "warning";

const STEP_STATUS: Record<string, { label: string; variant: StatusVariant; icon: typeof Circle; tone: string }> = {
  completed: { label: "Completed", variant: "success", icon: CheckCircle2, tone: "text-success" },
  failed: { label: "Failed", variant: "destructive", icon: XCircle, tone: "text-destructive" },
  paused: { label: "Paused", variant: "warning", icon: Pause, tone: "text-warning" },
  skipped: { label: "Skipped", variant: "secondary", icon: SkipForward, tone: "text-muted-foreground" },
  running: { label: "Running", variant: "default", icon: Clock, tone: "text-primary" },
  pending: { label: "Pending", variant: "outline", icon: Circle, tone: "text-muted-foreground" },
};

interface StepTimelineProps {
  steps: TimelineStep[];
  selectedStepId: string | null;
  onSelectStep: (stepId: string) => void;
}

function ranOn(engine: string | null, model: string | null): string | null {
  if (!engine) return null;
  return model ? `${engine} (${model})` : engine;
}

export function StepTimeline({ steps, selectedStepId, onSelectStep }: StepTimelineProps) {
  if (steps.length === 0) {
    return (
      <div className="flex items-center justify-center py-8 text-sm text-muted-foreground">
        No steps recorded yet
      </div>
    );
  }

  return (
    <div className="relative space-y-0">
      {steps.map((step, idx) => {
        const cfg = STEP_STATUS[step.status] ?? STEP_STATUS["pending"]!;
        const Icon = cfg.icon;
        const isSelected = selectedStepId === step.step_id;
        const isLast = idx === steps.length - 1;
        const record = step.record;
        const engine = record ? ranOn(record.engine, record.model) : null;

        return (
          <button
            key={`${step.step_id}-${idx}`}
            type="button"
            onClick={() => onSelectStep(step.step_id)}
            className={`relative flex items-start gap-3 w-full text-left px-3 py-2.5 rounded-md transition-colors ${
              isSelected ? "bg-muted" : "hover:bg-muted/50"
            }`}
          >
            <div className="flex flex-col items-center shrink-0 pt-0.5">
              <Icon className={`h-4 w-4 ${cfg.tone}`} />
              {!isLast && <div className="w-px h-full min-h-[16px] bg-border mt-1" />}
            </div>

            <div className="flex-1 min-w-0 space-y-1">
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium truncate">{step.name}</span>
                <Badge variant={cfg.variant} className="text-[10px] shrink-0">
                  {cfg.label}
                </Badge>
                {record?.outcome && (
                  <span className="text-[10px] font-mono text-muted-foreground truncate">→ {record.outcome}</span>
                )}
              </div>
              {record && (
                <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
                  <span title={formatRelativeTime(record.started_at)}>{formatDateTime(record.started_at)}</span>
                  <span>{formatDuration(record.duration_ms)}</span>
                  {record.rounds > 0 && <span>{record.rounds} round{record.rounds === 1 ? "" : "s"}</span>}
                  {engine && <span className="font-mono">{engine}</span>}
                  {record.cost_usd > 0 && <span>{formatCost(record.cost_usd)}</span>}
                </div>
              )}
            </div>
          </button>
        );
      })}
    </div>
  );
}
