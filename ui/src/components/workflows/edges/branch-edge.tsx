import { memo } from "react";
import { getBezierPath, type EdgeProps } from "@xyflow/react";
import { Badge } from "@/components/ui/badge";

function BranchEdgeBase({
  id,
  sourceX,
  sourceY,
  targetX,
  targetY,
  sourcePosition,
  targetPosition,
  data,
  label,
}: EdgeProps) {
  const [edgePath, labelX, labelY] = getBezierPath({
    sourceX,
    sourceY,
    targetX,
    targetY,
    sourcePosition,
    targetPosition,
  });

  const displayLabel = (data?.when as string) ?? label?.toString() ?? "";

  return (
    <>
      <path
        id={id}
        d={edgePath}
        className="fill-none stroke-primary stroke-[1.5]"
        strokeDasharray="6 3"
        markerEnd="url(#branch-arrow)"
      />
      {displayLabel && (
        <foreignObject
          x={labelX - 45}
          y={labelY - 12}
          width={90}
          height={24}
          className="overflow-visible pointer-events-none"
        >
          <div className="flex justify-center">
            <Badge variant="secondary" className="text-[10px] whitespace-nowrap shadow-sm">
              {displayLabel}
            </Badge>
          </div>
        </foreignObject>
      )}
    </>
  );
}

export const BranchEdge = memo(BranchEdgeBase);
