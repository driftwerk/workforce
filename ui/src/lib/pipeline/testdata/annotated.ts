/** The example orders pipeline with the annotation rules revision 11 adds: a judged overlap warns, a resolved finding reads well. */

import type { PipelineDefinition } from "../definition";
import type { LineageBody, LineageEvent } from "../lineage";
import { orders } from "./example";

export const annotated: PipelineDefinition = {
  ...orders,
  events: {
    ...orders.events,
    "review/overlap-judged": [{ annotation: true, tone: "warn", label_field: "headline" }],
    "watch/resolved": [{ annotation: true, tone: "good", label: "resolved" }],
  },
};

/** The event `stage/event` recorded at `at`. */
export function event(at: string, key: string, detail: Record<string, unknown> = {}): LineageEvent {
  const [stage = "", name = ""] = key.split("/");
  return { at, stage, event: name, detail };
}

export function history(...events: LineageEvent[]): LineageBody {
  return { history: events };
}
