import { memo } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import type { StepNodeData } from "./step-node";
import { StepBadges } from "./step-badges";
import { stepPreview } from "@/lib/workflows/step-kind";
import { Badge } from "@/components/ui/badge";
import { decisionBadge } from "@/lib/workflows/decision";

function PauseNodeBase({ data, selected }: NodeProps) {
  const { step, index, total } = data as unknown as StepNodeData;
  const taskPreview = stepPreview(step);

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-primary !w-2 !h-2" />
      <div
        className={`bg-card border-2 border-warning/60 rounded-lg p-3 w-[270px] shadow-sm animate-pulse-subtle ${
          selected ? "ring-2 ring-primary shadow-md" : ""
        }`}
      >
        <div className="flex items-start justify-between gap-2 mb-1.5">
          <div className="min-w-0 flex-1">
            <div className="font-semibold text-sm truncate flex items-center gap-1.5">
              <span>⏸</span>
              <span>{step.name}</span>
            </div>
            <div className="text-xs text-muted-foreground font-mono truncate">{step.id}</div>
          </div>
          <Badge variant="secondary" className="text-[10px] shrink-0">
            {index + 1}/{total}
          </Badge>
        </div>
        <p className="text-xs text-muted-foreground line-clamp-2 leading-relaxed">{taskPreview}</p>
        <StepBadges step={step}>
          {step.pause && (
            <Badge variant="warning" className="text-[10px]">{step.pause}</Badge>
          )}
          {step.decision && (
            <Badge variant="secondary" className="text-[10px]">{decisionBadge(step.decision)}</Badge>
          )}
        </StepBadges>
      </div>
      <Handle type="source" position={Position.Bottom} className="!bg-primary !w-2 !h-2" />
    </>
  );
}

export const PauseNode = memo(PauseNodeBase);
