/**
 * Pure readings of an item's costs for the item panel: engine names as a
 * person knows them and the notes that say what the figures leave out.
 */

import { apiErrorMessage } from "@/lib/api-error";
import type { Coverage, ItemCosts } from "@/lib/api/pipelines";

const ENGINE_NAMES: Record<string, string> = {
  "claude-code": "Claude subscription",
  "claude-code:bedrock": "Bedrock",
  codex: "Codex (ChatGPT)",
  codex_api: "Codex API",
  model: "Hub agent",
  agent: "Hub agent",
  "claude-pilot": "Claude Pilot",
  wasm: "Tool",
  report: "Report",
};

/** An engine label as a person reads it; an unknown label is shown as it is. */
export function engineName(engine: string | null): string {
  return engine === null ? "engine not recorded" : (ENGINE_NAMES[engine] ?? engine);
}

interface RanOn {
  engine: string | null;
  model: string | null;
}

/** A model usage: the model, then the engine it ran on. */
export function modelChip({ engine, model }: RanOn): string {
  return `${model ?? "model not reported"} · ${engineName(engine)}`;
}

/** What ran a step; tool and report steps name no model, so they show the engine alone. */
export function engineAndModel({ engine, model }: RanOn): string {
  return model ? modelChip({ engine, model }) : engineName(engine);
}

const IN_FLIGHT = new Set(["queued", "running", "paused"]);

/** Whether a run counted on the item is still going, so its figures are so far. */
export function stillRunning(costs: ItemCosts): boolean {
  return costs.runs.some((run) => run.booked_to === costs.item && IN_FLIGHT.has(run.status));
}

export function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** A sentence on what the figures do not cover; `warn` when it names runs that could not be matched. */
export interface CoverageNote {
  text: string;
  warn: boolean;
}

/** Whether the item's record names runs and none of them could be matched, so there are no figures to show. */
export function nothingMatched(c: Coverage): boolean {
  return c.runs_named > 0 && c.runs_unmatched === c.runs_named;
}

/** One note for each thing the figures do not cover, the warning first. */
export function coverageNotes({ coverage: c, scope }: ItemCosts, itemLabel: string): CoverageNote[] {
  const unmatchedExamples = scope.startsWith("agent:")
    ? "a chat session, a deleted run, or a run of a workflow that does not run as this pipeline's agent"
    : "a chat session or a deleted run";
  return [
    c.runs_unmatched > 0 && {
      text: `${count(c.runs_unmatched, "run", "runs")} this ${itemLabel} names could not be matched to a workflow run this pipeline counts (such as ${unmatchedExamples}), so ${
        c.runs_unmatched === 1 ? "its" : "their"
      } cost is not included`,
      warn: true,
    },
    c.steps_unrecorded > 0 && {
      text: `${count(c.steps_unrecorded, "step", "steps")} ran before costs were recorded, so ${
        c.steps_unrecorded === 1 ? "its cost is" : "their cost is"
      } not shown`,
      warn: false,
    },
    c.runs_shared > 0 && {
      text: `${count(c.runs_shared, "run is", "runs are")} counted on another ${itemLabel} that also recorded ${
        c.runs_shared === 1 ? "it" : "them"
      }`,
      warn: false,
    },
    c.history_truncated && { text: "Older history was trimmed; runs it named may be missing", warn: false },
  ].filter((note): note is CoverageNote => note !== false);
}

/** Why the costs could not be shown, from the error the request failed with. */
export function costsErrorText(error: unknown): string {
  const status = (error as { status?: number } | null)?.status;
  if (status === 503) return "Costs are unavailable: telemetry is not configured on this hub";
  if (status === 403) return "Costs are not visible to this login";
  return `Costs could not be loaded: ${apiErrorMessage(error)}`;
}
