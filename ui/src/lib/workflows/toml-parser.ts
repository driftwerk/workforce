import { parse as parseToml } from "smol-toml";
import type { Node as RFNode, Edge as RFEdge } from "@xyflow/react";
import type { WorkflowDefinition, StepDefinition } from "@/lib/api/workflows";

export interface ParseResult {
  def: WorkflowDefinition | null;
  error: string | null;
  errorLine: number | null;
}

/** Parse a TOML string into a typed WorkflowDefinition. */
export function parseWorkflowToml(src: string): ParseResult {
  try {
    const raw = parseToml(src) as unknown as WorkflowDefinition;

    if (!raw.workflow) {
      return { def: null, error: "Missing [workflow] section", errorLine: 1 };
    }

    // Normalise defaults
    const def: WorkflowDefinition = {
      workflow: {
        // Spread first, as for the steps: workflow-level fields the editor
        // doesn't type (max_concurrent, requires, steerable_by_others, …)
        // ride along so a visual edit can't reset them.
        ...raw.workflow,
        id: raw.workflow.id ?? "",
        name: raw.workflow.name ?? "",
        description: raw.workflow.description ?? "",
        version: raw.workflow.version ?? "1.0",
        agent: raw.workflow.agent,
        context: raw.workflow.context ?? {},
        delivery: raw.workflow.delivery,
      },
      step: (raw.step ?? []).map((s) => ({
        // Spread first: fields the editor doesn't type (report, tool, model,
        // phases, skills, …) ride along so a visual edit can't strip them.
        ...s,
        id: s.id ?? "",
        name: s.name ?? "",
        task: typeof s.task === "string" ? s.task : "",
        branches: (s.branches ?? []).map((b) => ({ when: b.when, goto: b.goto })),
        goto: s.goto,
        pause: s.pause,
        exit: s.exit ?? false,
        handoff: s.handoff,
        skill: s.skill,
        agent: s.agent,
        outputs: (s.outputs ?? []).map((o) => ({
          // Spread first, as for the step: `type` and `required` ride along.
          ...o,
          name: o.name,
          description: o.description ?? "",
          validate: o.validate,
        })),
      })),
      // Preserved verbatim — the visual editor edits steps, not triggers, so
      // every field (secret, schedule, http auth, …) survives a round-trip.
      trigger: raw.trigger ?? [],
      // Same for typed [[variables]] (incl. secret declarations): the editor
      // doesn't touch them, so carry them through losslessly.
      variables: raw.variables ?? [],
    };

    return { def, error: null, errorLine: null };
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    // smol-toml errors often contain line info like "at row X, col Y"
    const lineMatch = msg.match(/row\s+(\d+)/i);
    const errorLine = lineMatch ? parseInt(lineMatch[1]!, 10) : null;
    return { def: null, error: msg, errorLine };
  }
}

/** Determine node type based on step properties. */
function getNodeType(step: StepDefinition): string {
  if (step.exit) return "exitStep";
  if (step.pause) return "pauseStep";
  if (step.branches.length > 0) return "branchStep";
  return "normalStep";
}

/** Convert a parsed WorkflowDefinition to React Flow nodes and edges. */
export function tomlToGraph(def: WorkflowDefinition): { nodes: RFNode[]; edges: RFEdge[] } {
  const steps = def.step;
  const nodes: RFNode[] = steps.map((step, idx) => ({
    id: step.id,
    type: getNodeType(step),
    position: { x: 0, y: 0 },
    data: {
      step,
      index: idx,
      total: steps.length,
    },
  }));

  const edges: RFEdge[] = [];
  const stepIds = new Set(steps.map((s) => s.id));

  steps.forEach((step, idx) => {
    // Branch edges
    for (const branch of step.branches) {
      if (stepIds.has(branch.goto)) {
        edges.push({
          id: `${step.id}-branch-${branch.when}`,
          source: step.id,
          target: branch.goto,
          type: "branchEdge",
          label: branch.when,
          data: { when: branch.when },
        });
      }
    }

    // Goto edge (unconditional jump)
    if (step.goto && stepIds.has(step.goto)) {
      edges.push({
        id: `${step.id}-goto-${step.goto}`,
        source: step.id,
        target: step.goto,
        type: "gotoEdge",
        data: { label: "goto" },
      });
    }

    // A decision button that sends the run to a step
    for (const button of step.decision?.buttons ?? []) {
      if (button.goto && stepIds.has(button.goto)) {
        edges.push({
          id: `${step.id}-decision-${button.outcome}`,
          source: step.id,
          target: button.goto,
          type: "decisionEdge",
          label: button.label,
          data: { when: button.label },
        });
      }
    }

    // Sequential fall-through: step[i] → step[i+1]
    // Only if no goto, not an exit, not a decision (its buttons route it),
    // and branches don't cover all paths
    const nextStep = steps[idx + 1];
    if (nextStep && !step.goto && !step.exit && !step.decision) {
      edges.push({
        id: `${step.id}-next-${nextStep.id}`,
        source: step.id,
        target: nextStep.id,
        type: "default",
        data: { sequential: true },
      });
    }
  });

  return { nodes, edges };
}
