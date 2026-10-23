import type { ReactNode } from "react";
import type { StepDefinition } from "@/lib/api/workflows";
import { stepKindLabel } from "@/lib/workflows/step-kind";
import { Badge } from "@/components/ui/badge";

/** The badge row under a step's preview: what runs without a model (a tool call, a decision, a child workflow or a rendered report), then the caller's own badges. Hidden when there is nothing to show. */
export function StepBadges({ step, children }: { step: StepDefinition; children?: ReactNode }) {
  const label = stepKindLabel(step);
  return (
    <div className="mt-1.5 flex flex-wrap gap-1 empty:hidden">
      {label != null && (
        <Badge variant="outline" className="text-[10px]">
          {label}
        </Badge>
      )}
      {children}
    </div>
  );
}
