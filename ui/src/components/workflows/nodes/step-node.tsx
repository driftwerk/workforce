import { memo } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import { StepBadges } from "./step-badges";
import type { StepDefinition } from "@/lib/api/workflows";
import { stepPreview } from "@/lib/workflows/step-kind";
import { Badge } from "@/components/ui/badge";

export interface StepNodeData {
  step: StepDefinition;
  index: number;
  total: number;
  [key: string]: unknown;
}

function StepNodeBase({ data, selected }: NodeProps) {
  const { step, index, total } = data as unknown as StepNodeData;
  const taskPreview = stepPreview(step);

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-primary !w-2 !h-2" />
      <div
        className={`bg-card border rounded-lg p-3 w-[270px] shadow-sm transition-shadow ${
          selected ? "ring-2 ring-primary shadow-md" : "border-border"
        }`}
      >
        <div className="flex items-start justify-between gap-2 mb-1.5">
          <div className="min-w-0 flex-1">
            <div className="font-semibold text-sm truncate">{step.name}</div>
            <div className="text-xs text-muted-foreground font-mono truncate">{step.id}</div>
          </div>
          <Badge variant="secondary" className="text-[10px] shrink-0">
            {index + 1}/{total}
          </Badge>
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

export const StepNode = memo(StepNodeBase);
