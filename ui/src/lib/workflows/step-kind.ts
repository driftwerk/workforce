import type { StepDefinition } from "@/lib/api/workflows";

export type StepKind = "agent" | "tool" | "decide" | "workflow" | "report";

/** Mirrors the engine: `tool`, `decide` or `workflow` set → that step; no task → report-only; else a model turn. */
export function stepKind(step: StepDefinition): StepKind {
  if (step.tool != null) return "tool";
  if (step.decide != null) return "decide";
  if (step.workflow != null) return "workflow";
  if (step.task.trim() === "") return "report";
  return "agent";
}

/** The badge text for a step that runs without a model turn; `null` for a model turn. */
export function stepKindLabel(step: StepDefinition): string | null {
  switch (stepKind(step)) {
    case "tool":
      return `tool: ${step.tool}`;
    case "decide":
      return `decide: ${step.decide?.kind ?? "choice"}`;
    case "workflow":
      return `workflow: ${typeof step.workflow === "string" ? step.workflow : step.workflow?.id}`;
    case "report":
      return "report";
    case "agent":
      return null;
  }
}

const PREVIEW_CHARS = 100;

/** What a node shows under the step name: the report a report-only step renders, else its task. */
export function stepPreview(step: StepDefinition): string {
  const text = stepKind(step) === "report" ? (step.report ?? "") : step.task;
  return text.length > PREVIEW_CHARS ? `${text.slice(0, PREVIEW_CHARS)}…` : text;
}
