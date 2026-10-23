import { memo } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import { StepBadges } from "./step-badges";
import type { StepDefinition } from "@/lib/api/workflows";
import { stepPreview } from "@/lib/workflows/step-kind";
import { Badge } from "@/components/ui/badge";

export interface InstanceStepNodeData {
  step: StepDefinition;
  index: number;
  total: number;
  instanceStatus: string;
  isCurrent: boolean;
  [key: string]: unknown;
}

const STATUS_BORDER: Record<string, string> = {
  completed: "border-success/60",
  failed: "border-destructive/60",
  paused: "border-warning/60",
  skipped: "border-muted-foreground/40",
  running: "border-primary/60",
  pending: "border-border",
};

const STATUS_BG: Record<string, string> = {
  completed: "bg-success/5",
  failed: "bg-destructive/5",
  paused: "bg-warning/5",
  skipped: "bg-muted/50",
  running: "bg-primary/5",
  pending: "bg-card",
};

type BadgeVariant = "default" | "secondary" | "destructive" | "outline" | "success" | "warning";

const STATUS_BADGE: Record<string, { label: string; variant: BadgeVariant }> = {
  completed: { label: "Done", variant: "success" },
  failed: { label: "Failed", variant: "destructive" },
  paused: { label: "Paused", variant: "warning" },
  skipped: { label: "Skipped", variant: "secondary" },
  running: { label: "Running", variant: "default" },
  pending: { label: "Pending", variant: "outline" },
};

function InstanceStepNodeBase({ data }: NodeProps) {
  const { step, index, total, instanceStatus, isCurrent } = data as unknown as InstanceStepNodeData;
  const taskPreview = stepPreview(step);
  const borderColor = STATUS_BORDER[instanceStatus] ?? STATUS_BORDER["pending"]!;
  const bgColor = STATUS_BG[instanceStatus] ?? STATUS_BG["pending"]!;
  const badge = STATUS_BADGE[instanceStatus] ?? STATUS_BADGE["pending"]!;

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-primary !w-2 !h-2" />
      <div
        className={`border-2 rounded-lg p-3 w-[270px] shadow-sm transition-all ${borderColor} ${bgColor} ${
          isCurrent ? "ring-2 ring-primary ring-offset-2 ring-offset-background shadow-lg" : ""
        }`}
      >
        <div className="flex items-start justify-between gap-2 mb-1.5">
          <div className="min-w-0 flex-1">
            <div className="font-semibold text-sm truncate">{step.name}</div>
            <div className="text-xs text-muted-foreground font-mono truncate">{step.id}</div>
          </div>
          <div className="flex items-center gap-1 shrink-0">
            <Badge variant={badge.variant} className="text-[10px]">{badge.label}</Badge>
            <Badge variant="secondary" className="text-[10px]">
              {index + 1}/{total}
            </Badge>
          </div>
        </div>
        <p className="text-xs text-muted-foreground line-clamp-2 leading-relaxed">{taskPreview}</p>
        <StepBadges step={step}>
          {step.agent && (
            <Badge variant="outline" className="text-[10px]">agent: {step.agent}</Badge>
          )}
        </StepBadges>
      </div>
      <Handle type="source" position={Position.Bottom} className="!bg-primary !w-2 !h-2" />
    </>
  );
}

export const InstanceStepNode = memo(InstanceStepNodeBase);
