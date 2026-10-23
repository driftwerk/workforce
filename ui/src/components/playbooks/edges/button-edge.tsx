import { memo } from "react";
import { getBezierPath, type EdgeProps } from "@xyflow/react";
import type { ButtonStyle } from "@/lib/api/playbooks";

/** A labeled edge for a custom button — the label is the button text, coloured
 *  by its Slack style; a dangling `goto` target is drawn red. */
function ButtonEdgeBase({
  id,
  sourceX,
  sourceY,
  targetX,
  targetY,
  sourcePosition,
  targetPosition,
  data,
}: EdgeProps) {
  const [edgePath, labelX, labelY] = getBezierPath({
    sourceX,
    sourceY,
    targetX,
    targetY,
    sourcePosition,
    targetPosition,
  });

  const style = (data?.style as ButtonStyle) ?? "default";
  const error = data?.error === true;
  const label = (data?.label as string) ?? "";

  const stroke = error
    ? "stroke-red-500"
    : style === "primary"
      ? "stroke-emerald-600"
      : style === "danger"
        ? "stroke-rose-600"
        : "stroke-muted-foreground";
  const chip = error
    ? "bg-red-500 text-white"
    : style === "primary"
      ? "bg-emerald-600 text-white"
      : style === "danger"
        ? "bg-rose-600 text-white"
        : "bg-muted text-foreground border border-border";

  return (
    <>
      <path
        id={id}
        d={edgePath}
        className={`fill-none ${stroke} stroke-[1.5]`}
        strokeDasharray="6 3"
        markerEnd={error ? "url(#pb-arrow-error)" : "url(#pb-arrow)"}
      />
      {label && (
        <foreignObject
          x={labelX - 50}
          y={labelY - 12}
          width={100}
          height={24}
          className="overflow-visible pointer-events-none"
        >
          <div className="flex justify-center">
            <span className={`rounded px-1.5 py-0.5 text-[10px] font-medium shadow-sm whitespace-nowrap ${chip}`}>
              {label}
            </span>
          </div>
        </foreignObject>
      )}
    </>
  );
}

export const ButtonEdge = memo(ButtonEdgeBase);
