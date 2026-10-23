import { parse as parseToml } from "smol-toml";
import type { Node as RFNode, Edge as RFEdge } from "@xyflow/react";
import type { ButtonStyle, PlaybookDefinition } from "@/lib/api/playbooks";

export interface ParseResult {
  def: PlaybookDefinition | null;
  error: string | null;
  errorLine: number | null;
}

// Raw TOML shape (snake_case `keep_continue`, `[[step.button]]`) before we
// normalise it into the camelCase typed definition.
interface RawButton {
  label?: string;
  style?: string;
  goto?: string;
  exit?: boolean;
  finish?: boolean;
}
interface RawStep {
  id?: string;
  title?: string;
  prompt?: string;
  artifact?: string;
  keep_continue?: boolean;
  model?: string;
  button?: RawButton[];
}
interface RawPlaybook {
  playbook?: { id?: string; name?: string; description?: string };
  step?: RawStep[];
}

/** Parse a TOML string into a typed PlaybookDefinition. */
export function parsePlaybookToml(src: string): ParseResult {
  try {
    const raw = parseToml(src) as unknown as RawPlaybook;

    if (!raw.playbook) {
      return { def: null, error: "Missing [playbook] section", errorLine: 1 };
    }

    const def: PlaybookDefinition = {
      playbook: {
        id: raw.playbook.id ?? "",
        name: raw.playbook.name ?? "",
        description: raw.playbook.description ?? "",
      },
      step: (raw.step ?? []).map((s) => ({
        id: s.id ?? "",
        title: s.title ?? "",
        prompt: s.prompt ?? "",
        artifact: s.artifact,
        model: s.model,
        keepContinue: s.keep_continue ?? true,
        buttons: (s.button ?? []).map((b) => ({
          label: b.label ?? "",
          style: (b.style ?? "default") as ButtonStyle,
          goto: b.goto,
          exit: b.exit ?? false,
          finish: b.finish ?? false,
        })),
      })),
    };

    return { def, error: null, errorLine: null };
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    const lineMatch = msg.match(/row\s+(\d+)/i);
    const errorLine = lineMatch ? parseInt(lineMatch[1]!, 10) : null;
    return { def: null, error: msg, errorLine };
  }
}

/** A synthetic terminal/error node a button edge points at. */
export type TerminalKind = "finish" | "exit" | "missing";
export interface TerminalNodeData {
  kind: TerminalKind;
  label: string;
  [key: string]: unknown;
}

/**
 * Convert a parsed PlaybookDefinition to React Flow nodes and edges.
 *
 * Edges model the playbook's control flow: a solid `default` edge is the linear
 * `Continue` fall-through (dropped when `keepContinue` is false), and each
 * custom button is a labeled `playbookButton` edge to its target step — or to a
 * synthetic terminal node for `exit` / `finish`, or a red "missing" node when a
 * `goto` points at a step that doesn't exist (so a mid-edit typo is visible
 * rather than crashing the graph).
 */
export function playbookTomlToGraph(def: PlaybookDefinition): {
  nodes: RFNode[];
  edges: RFEdge[];
} {
  const steps = def.step;
  const stepIds = new Set(steps.map((s) => s.id));

  const nodes: RFNode[] = steps.map((step, index) => ({
    id: step.id,
    type: "playbookStep",
    position: { x: 0, y: 0 },
    data: { step, index, total: steps.length },
  }));

  const terminals = new Map<string, TerminalNodeData>();
  const ensureTerminal = (id: string, data: TerminalNodeData) => {
    if (!terminals.has(id)) terminals.set(id, data);
  };

  const edges: RFEdge[] = [];

  steps.forEach((step, idx) => {
    // Linear Continue edge to the next step.
    const next = steps[idx + 1];
    if (step.keepContinue && next) {
      edges.push({
        id: `${step.id}-continue`,
        source: step.id,
        target: next.id,
        type: "default",
        data: { sequential: true },
      });
    }

    // Custom branching buttons.
    step.buttons.forEach((b, bi) => {
      const id = `${step.id}-btn-${bi}`;
      const data = { label: b.label, style: b.style };
      // Synthetic terminal node ids use a `$` prefix, which the backend forbids
      // in step ids — so they can never collide with a real step's node.
      if (b.finish) {
        ensureTerminal("$finish", { kind: "finish", label: "Finish" });
        edges.push({ id, source: step.id, target: "$finish", type: "playbookButton", data });
      } else if (b.exit) {
        ensureTerminal("$exit", { kind: "exit", label: "Exit" });
        edges.push({ id, source: step.id, target: "$exit", type: "playbookButton", data });
      } else if (b.goto && stepIds.has(b.goto)) {
        edges.push({ id, source: step.id, target: b.goto, type: "playbookButton", data });
      } else if (b.goto) {
        const missingId = `$missing:${b.goto}`;
        ensureTerminal(missingId, { kind: "missing", label: b.goto });
        edges.push({
          id,
          source: step.id,
          target: missingId,
          type: "playbookButton",
          data: { ...data, error: true },
        });
      }
    });
  });

  for (const [id, data] of terminals) {
    nodes.push({ id, type: "playbookTerminal", position: { x: 0, y: 0 }, data });
  }

  return { nodes, edges };
}
