// Dispatch a wf-table / wf-chart fence to its renderer. The heavy components
// are lazy chunks; until a payload parses AND validates the block shows a
// skeleton (while the fence is still streaming) or falls back to the original
// code block (once the message is final) — invalid agent output must always
// degrade to something legible, never crash or blank.

import { lazy, Suspense, useMemo, type ReactNode } from "react";

import type { RichKind } from "./fence";
import { retryImport } from "./lazyRetry";
import {
  parsePayload,
  validateChart,
  validateDiagram,
  validateForm,
  validateImage,
  validateMap,
  validateTable,
} from "./spec";

// Retry the chunk fetch: a transient failure (deploy rollover, flaky network)
// must not permanently dead-end the message with the ErrorBoundary.
const WfTable = lazy(() => retryImport(() => import("./WfTable")));
const WfChart = lazy(() => retryImport(() => import("./WfChart")));
const WfForm = lazy(() => retryImport(() => import("./WfForm")));
const WfMap = lazy(() => retryImport(() => import("./WfMap")));
const WfDiagram = lazy(() => retryImport(() => import("./WfDiagram")));
const WfImage = lazy(() => retryImport(() => import("./WfImage")));

const SKELETON_LABEL: Record<RichKind, string> = {
  table: "Preparing table…",
  chart: "Preparing chart…",
  form: "Preparing form…",
  map: "Preparing map…",
  diagram: "Preparing diagram…",
  image: "Loading image…",
};

function Skeleton({ kind }: { kind: RichKind }) {
  return (
    <div className={"wf-skel wf-skel-" + kind} role="status">
      <span className="wf-skel-label">{SKELETON_LABEL[kind]}</span>
    </div>
  );
}

export function RichBlock({
  kind,
  payload,
  streaming,
  fallback,
  excalidraw = false,
}: {
  kind: RichKind;
  payload: string;
  streaming: boolean;
  fallback: ReactNode;
  /** Whether a diagram offers the Excalidraw download. Image download is
   *  unconditional, so only this one is threaded through. */
  excalidraw?: boolean;
}) {
  const validated = useMemo(() => {
    const parsed = parsePayload(payload);
    if (parsed === null) return null;
    if (kind === "table") return validateTable(parsed);
    if (kind === "chart") return validateChart(parsed);
    if (kind === "map") return validateMap(parsed);
    if (kind === "diagram") return validateDiagram(parsed);
    if (kind === "image") return validateImage(parsed);
    return validateForm(parsed);
  }, [kind, payload]);

  // A form must never be fillable mid-stream: even a spec that happens to parse
  // stays a skeleton until the message is final (unlike tables/charts, which
  // render live). Once final, an invalid form degrades to the code block. A
  // diagram waits for the same reason plus one of its own: a half-streamed graph
  // is missing edges, which reads as a DIFFERENT process rather than a shorter
  // one, and laying it out per chunk is the most expensive work in the family.
  if ((kind === "form" || kind === "diagram") && streaming) return <Skeleton kind={kind} />;
  if (!validated?.ok) {
    return streaming ? <Skeleton kind={kind} /> : <>{fallback}</>;
  }
  const spec = validated.spec;
  if ("fields" in spec) {
    return (
      <Suspense fallback={<Skeleton kind="form" />}>
        <WfForm spec={spec} />
      </Suspense>
    );
  }
  // The chart arm is the unguarded fallback, so every other spec shape must be
  // discriminated before it.
  return (
    <Suspense fallback={<Skeleton kind={kind} />}>
      {"mode" in spec ? (
        <WfTable spec={spec} truncated={validated.truncated} />
      ) : "regions" in spec ? (
        <WfMap spec={spec} />
      ) : "images" in spec ? (
        <WfImage spec={spec} />
      ) : "nodes" in spec || "participants" in spec ? (
        <WfDiagram spec={spec} excalidraw={excalidraw} />
      ) : (
        <WfChart spec={spec} />
      )}
    </Suspense>
  );
}
