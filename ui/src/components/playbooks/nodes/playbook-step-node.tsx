import { memo } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import type { ButtonStyle, PlaybookStep } from "@/lib/api/playbooks";
import { Badge } from "@/components/ui/badge";

export interface PlaybookStepNodeData {
  step: PlaybookStep;
  index: number;
  total: number;
  [key: string]: unknown;
}

/** Faithful preview of a Slack button — the three Block Kit styles are all
 *  Slack offers (grey default, green primary, red danger); the label carries any
 *  emoji verbatim. */
function ButtonChip({ label, style }: { label: string; style: ButtonStyle }) {
  const cls =
    style === "primary"
      ? "bg-emerald-600 text-white"
      : style === "danger"
        ? "bg-rose-600 text-white"
        : "bg-muted text-foreground border border-border";
  return (
    <span className={`rounded px-2 py-0.5 text-[10px] font-medium ${cls}`}>{label || "—"}</span>
  );
}

function PlaybookStepNodeBase({ data, selected }: NodeProps) {
  const { step, index, total } = data as unknown as PlaybookStepNodeData;
  const promptPreview = step.prompt.length > 100 ? step.prompt.slice(0, 100) + "…" : step.prompt;
  const isLast = index + 1 >= total;

  // Mirror the backend control row: forced-choice → Exit only; last step →
  // lone Finish; otherwise Continue + Exit.
  const controls: { label: string; style: ButtonStyle }[] = !step.keepContinue
    ? [{ label: "Exit", style: "danger" }]
    : isLast
      ? [{ label: "Finish", style: "primary" }]
      : [
          { label: "Continue", style: "primary" },
          { label: "Exit", style: "danger" },
        ];

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
            <div className="font-semibold text-sm truncate">{step.title}</div>
            <div className="text-xs text-muted-foreground font-mono truncate">{step.id}</div>
          </div>
          <Badge variant="secondary" className="text-[10px] shrink-0">
            {index + 1}/{total}
          </Badge>
        </div>
        <p className="text-xs text-muted-foreground line-clamp-2 leading-relaxed">{promptPreview}</p>
        {(step.artifact || step.model) && (
          <div className="mt-1.5 flex flex-wrap gap-1">
            {step.artifact && (
              <Badge variant="outline" className="text-[10px]">
                ⎙ {step.artifact}
              </Badge>
            )}
            {step.model && (
              <Badge variant="outline" className="text-[10px] font-mono">
                ⚙ {step.model}
              </Badge>
            )}
          </div>
        )}
        <div className="mt-2 border-t border-border pt-2 space-y-1">
          {step.buttons.length > 0 && (
            <div className="flex flex-wrap gap-1">
              {step.buttons.map((b, i) => (
                <ButtonChip key={i} label={b.label} style={b.style} />
              ))}
            </div>
          )}
          <div className="flex flex-wrap gap-1">
            {controls.map((c) => (
              <ButtonChip key={c.label} label={c.label} style={c.style} />
            ))}
          </div>
        </div>
      </div>
      <Handle type="source" position={Position.Bottom} className="!bg-primary !w-2 !h-2" />
    </>
  );
}

export const PlaybookStepNode = memo(PlaybookStepNodeBase);
