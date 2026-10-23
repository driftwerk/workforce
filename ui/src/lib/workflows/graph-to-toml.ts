import { stringify } from "smol-toml";
import type { Node, Edge } from "@xyflow/react";
import type {
  WorkflowDefinition,
  WorkflowMeta,
  StepDefinition,
  Branch,
  TriggerDefinition,
} from "@/lib/api/workflows";
import { escapeTomlString } from "@/lib/toml-util";

/** The `[workflow]` fields written by hand below; the rest go verbatim. */
const TYPED_META = new Set(["id", "name", "description", "version", "agent", "delivery", "context"]);

/**
 * Reconstruct a WorkflowDefinition from React Flow graph state. Triggers are
 * carried through unchanged — the graph edits steps, not triggers.
 */
export function graphToWorkflowDef(
  nodes: Node[],
  edges: Edge[],
  meta: WorkflowMeta,
  triggers: TriggerDefinition[] = [],
): WorkflowDefinition {
  // Sort nodes by Y position to determine step order
  const sortedNodes = [...nodes].sort((a, b) => a.position.y - b.position.y);

  const steps: StepDefinition[] = sortedNodes.map((node) => {
    const step = node.data.step as StepDefinition;

    // Rebuild branches from edges
    const branchEdges = edges.filter(
      (e) => e.source === node.id && e.type === "branchEdge",
    );
    const branches: Branch[] = branchEdges.map((e) => ({
      when: (e.data?.when as string) ?? e.label?.toString() ?? "",
      goto: e.target,
    }));

    // Rebuild goto from edges
    const gotoEdge = edges.find(
      (e) => e.source === node.id && e.type === "gotoEdge",
    );

    return {
      ...step,
      branches: branches.length > 0 ? branches : step.branches,
      goto: gotoEdge ? gotoEdge.target : step.goto,
    };
  });

  return {
    workflow: meta,
    step: steps,
    trigger: triggers,
  };
}

/** Serialise a WorkflowDefinition to TOML string. */
export function workflowDefToToml(def: WorkflowDefinition): string {
  const lines: string[] = [];

  // [workflow] section
  lines.push("[workflow]");
  lines.push(`id = "${escapeTomlString(def.workflow.id)}"`);
  lines.push(`name = "${escapeTomlString(def.workflow.name)}"`);
  if (def.workflow.description) {
    lines.push(`description = "${escapeTomlString(def.workflow.description)}"`);
  }
  if (def.workflow.version && def.workflow.version !== "1.0") {
    lines.push(`version = "${escapeTomlString(def.workflow.version)}"`);
  }
  if (def.workflow.agent) {
    lines.push(`agent = "${escapeTomlString(def.workflow.agent)}"`);
  }
  if (def.workflow.delivery && def.workflow.delivery !== "full") {
    lines.push(`delivery = "${escapeTomlString(def.workflow.delivery)}"`);
  }
  // Every other workflow-level field, verbatim, so none is lost on a save.
  const untyped = Object.fromEntries(
    Object.entries(def.workflow).filter(([key, value]) => !TYPED_META.has(key) && value !== undefined),
  );
  if (Object.keys(untyped).length > 0) {
    const [header, ...body] = stringify({ workflow: untyped }).trimEnd().split("\n");
    lines.push(...(header === "[workflow]" ? body : [header!, ...body]));
  }

  // Context fields
  const contextEntries = Object.entries(def.workflow.context);
  if (contextEntries.length > 0) {
    lines.push("");
    lines.push("[workflow.context]");
    for (const [key, field] of contextEntries) {
      const parts: string[] = [];
      if (field.required) parts.push("required = true");
      if (field.description) parts.push(`description = "${escapeTomlString(field.description)}"`);
      lines.push(`${key} = { ${parts.join(", ")} }`);
    }
  }

  // [[step]] blocks — serialised verbatim via the TOML library (like the
  // triggers and variables below) so every field — report, tool, model,
  // phases, skills, … — round-trips losslessly through the visual editor.
  // Undefined values (e.g. a goto the graph never set) are stripped first;
  // the TOML writer rejects them.
  if (def.step.length > 0) {
    const steps = JSON.parse(JSON.stringify(def.step)) as unknown[];
    lines.push("");
    lines.push(stringify({ step: steps }).trimEnd());
  }

  // [[trigger]] blocks — serialised verbatim via the TOML library so every
  // field (webhook secret, schedule cron, http auth/response, …) and any
  // future field round-trips losslessly.
  const triggers = def.trigger ?? [];
  if (triggers.length > 0) {
    lines.push("");
    lines.push(stringify({ trigger: triggers }).trimEnd());
  }

  // [[variables]] blocks (typed inputs incl. secret declarations) — likewise
  // serialised verbatim so the editor never strips them.
  const variables = def.variables ?? [];
  if (variables.length > 0) {
    lines.push("");
    lines.push(stringify({ variables }).trimEnd());
  }

  lines.push("");
  return lines.join("\n");
}
