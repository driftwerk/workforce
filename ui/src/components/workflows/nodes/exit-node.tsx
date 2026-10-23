import { memo } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import type { StepNodeData } from "./step-node";
import { StepBadges } from "./step-badges";
import { stepPreview } from "@/lib/workflows/step-kind";
import { Badge } from "@/components/ui/badge";

function ExitNodeBase({ data, selected }: NodeProps) {
  const { step, index, total } = data as unknown as StepNodeData;
  const taskPreview = stepPreview(step);

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-primary !w-2 !h-2" />
      <div
        className={`bg-card border-2 border-success/60 rounded-lg p-3 w-[270px] shadow-sm ${
          selected ? "ring-2 ring-primary shadow-md" : ""
        }`}
      >
        <div className="flex items-start justify-between gap-2 mb-1.5">
          <div className="min-w-0 flex-1">
            <div className="font-semibold text-sm truncate flex items-center gap-1.5">
              <span>🏁</span>
              <span>{step.name}</span>
            </div>
            <div className="text-xs text-muted-foreground font-mono truncate">{step.id}</div>
          </div>
          <div className="flex items-center gap-1 shrink-0">
            <Badge variant="success" className="text-[10px]">Terminal</Badge>
            <Badge variant="secondary" className="text-[10px]">
              {index + 1}/{total}
            </Badge>
          </div>
        </div>
        <p className="text-xs text-muted-foreground line-clamp-2 leading-relaxed">{taskPreview}</p>
        <StepBadges step={step}>
          {step.handoff && (
            <Badge variant="outline" className="text-[10px]">handoff: {step.handoff}</Badge>
          )}
        </StepBadges>
      </div>
    </>
  );
}

export const ExitNode = memo(ExitNodeBase);
