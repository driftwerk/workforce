import { memo } from "react";
import { getSmoothStepPath, type EdgeProps } from "@xyflow/react";

function GotoEdgeBase({
  id,
  sourceX,
  sourceY,
  targetX,
  targetY,
  sourcePosition,
  targetPosition,
}: EdgeProps) {
  const [edgePath, labelX, labelY] = getSmoothStepPath({
    sourceX,
    sourceY,
    targetX,
    targetY,
    sourcePosition,
    targetPosition,
  });

  return (
    <>
      <path
        id={id}
        d={edgePath}
        className="fill-none stroke-muted-foreground/50 stroke-[1.5]"
        strokeDasharray="4 4"
        markerEnd="url(#goto-arrow)"
      />
      <foreignObject
        x={labelX - 22}
        y={labelY - 10}
        width={44}
        height={20}
        className="overflow-visible pointer-events-none"
      >
        <div className="flex justify-center">
          <span className="text-[10px] text-muted-foreground bg-background px-1.5 py-0.5 rounded border border-border">
            goto
          </span>
        </div>
      </foreignObject>
    </>
  );
}

export const GotoEdge = memo(GotoEdgeBase);
