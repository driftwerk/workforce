import { useId, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { readStored, writeStored } from "@/lib/local-store";
import type { PipelineDefinition } from "@/lib/pipeline/definition";
import { BOX_H, BOX_W, arrowLabel, arrows, boxLines, edgePath, layout, type Arrow, type Box, type Diagram, type Edge } from "@/lib/pipeline/diagram";

const OPEN_KEY = (scope: string, id: string) => `pipeline-guide-open:${scope}/${id}`;

function readOpen(key: string): boolean {
  return readStored(key) === "1";
}

function writeOpen(key: string, open: boolean) {
  writeStored(key, open ? "1" : "0");
}

function edgeText(edge: Edge): string {
  return edge.when ? `${edge.key} when ${edge.when}` : edge.key;
}

function arrowTitle(arrow: Arrow): string {
  const rules = arrow.edges.map(edgeText).join("\n");
  return arrow.unknown ? `${rules}\nNo column "${arrow.to.id}" in this definition` : rules;
}

type BoxKind = "lane" | "exit" | "unknown";

const BOX_CLASS: Record<BoxKind, string> = {
  lane: "fill-card stroke-border",
  exit: "fill-muted/40 stroke-border",
  unknown: "fill-destructive/10 stroke-destructive",
};

function DiagramBox({ box, kind }: { box: Box; kind: BoxKind }) {
  const lines = boxLines(box.label);
  const firstY = box.y + BOX_H / 2 + 4 - (lines.length - 1) * 6;
  return (
    <g>
      <title>{box.reached ? box.label : `${box.label} · nothing lands here`}</title>
      <rect
        x={box.x}
        y={box.y}
        width={BOX_W}
        height={BOX_H}
        rx={8}
        strokeWidth={1}
        strokeDasharray={box.reached ? undefined : "4 3"}
        className={BOX_CLASS[kind]}
      />
      {lines.map((line, i) => (
        <text key={line} x={box.x + BOX_W / 2} y={firstY + i * 12} textAnchor="middle" className="fill-foreground font-medium">
          {line}
        </text>
      ))}
    </g>
  );
}

function FlowDiagram({ diagram }: { diagram: Diagram }) {
  const moving = arrows(diagram.edges);
  const stays = diagram.edges.filter((e) => e.stays);
  return (
    <svg
      viewBox={`0 0 ${diagram.width} ${diagram.height}`}
      className="h-auto w-full max-w-full text-[11px]"
      role="img"
      aria-label="Lanes left to right, exits below, one arrow per pair of columns an event joins"
    >
      <defs>
        <marker id="pipeline-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
          <path d="M 0 0 L 8 4 L 0 8 z" className="fill-muted-foreground" />
        </marker>
        <marker id="pipeline-arrow-bad" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="8" markerHeight="8" orient="auto-start-reverse">
          <path d="M 0 0 L 8 4 L 0 8 z" className="fill-destructive" />
        </marker>
      </defs>
      {moving.map((arrow) => (
        <g key={`${arrow.from.id}-${arrow.to.id}`}>
          <title>{arrowTitle(arrow)}</title>
          <path
            d={edgePath(arrow)}
            fill="none"
            strokeWidth={1.25}
            className={arrow.unknown ? "stroke-destructive" : "stroke-muted-foreground"}
            markerEnd={arrow.unknown ? "url(#pipeline-arrow-bad)" : "url(#pipeline-arrow)"}
          />
        </g>
      ))}
      {diagram.lanes.map((box) => (
        <DiagramBox key={box.id} box={box} kind="lane" />
      ))}
      {diagram.exits.map((box) => (
        <DiagramBox key={box.id} box={box} kind="exit" />
      ))}
      {diagram.unknownColumns.map((box) => (
        <DiagramBox key={box.id} box={box} kind="unknown" />
      ))}
      {diagram.lanes.map((lane) => {
        const own = [...new Set(stays.filter((e) => e.from.id === lane.id).map((e) => e.key))];
        return own.map((key, i) => (
          <text key={`${lane.id}-${key}`} x={lane.x + BOX_W / 2} y={lane.y + BOX_H + 14 + i * 12} textAnchor="middle" className="fill-muted-foreground">
            {key}
          </text>
        ));
      })}
      {moving.map((arrow) => {
        const x = (arrow.from.x + arrow.to.x) / 2 + BOX_W / 2;
        const y = arrow.to.y > arrow.from.y ? (arrow.from.y + BOX_H + arrow.to.y) / 2 : arrow.from.y - 6;
        return (
          <text key={`label-${arrow.from.id}-${arrow.to.id}`} x={x} y={y} textAnchor="middle" className={cn(arrow.unknown ? "fill-destructive" : "fill-muted-foreground")}>
            {arrowLabel(arrow)}
          </text>
        );
      })}
    </svg>
  );
}

function ColumnList({ columns, className }: { columns: { id: string; label: string }[]; className?: string }) {
  return (
    <ol className={cn("flex flex-wrap items-center gap-1 text-xs", className)}>
      {columns.map((c, i) => (
        <li key={c.id} className="flex items-center gap-1">
          {i > 0 && <span className="text-muted-foreground">→</span>}
          <span className="rounded border px-2 py-0.5">{c.label}</span>
        </li>
      ))}
    </ol>
  );
}

/**
 * How the pipeline works, for a person who was not there when it was built:
 * the flow drawn from the definition, then the definition's own guide.
 * Closed by default; the choice is remembered per pipeline in this browser.
 * Mount it keyed by scope and id so a new pipeline reads its own preference.
 */
export function GuidePanel({ def, scope, id }: { def: PipelineDefinition; scope: string; id: string }) {
  const key = OPEN_KEY(scope, id);
  const contentId = useId();
  const [open, setOpen] = useState(() => readOpen(key));
  const toggle = () => {
    setOpen((o) => {
      writeOpen(key, !o);
      return !o;
    });
  };
  const diagram = layout(def);

  return (
    <Card>
      <CardHeader className="pb-2">
        <button type="button" onClick={toggle} className="flex w-full items-center justify-between text-left" aria-expanded={open} aria-controls={contentId}>
          <CardTitle className="text-sm">How this pipeline works</CardTitle>
          {open ? <ChevronDown className="h-4 w-4 text-muted-foreground" /> : <ChevronRight className="h-4 w-4 text-muted-foreground" />}
        </button>
      </CardHeader>
      {open && (
        <CardContent id={contentId} className="space-y-4">
          <div className="overflow-x-auto rounded-md border bg-background/40 p-3">
            {diagram ? (
              <FlowDiagram diagram={diagram} />
            ) : (
              <div className="space-y-2">
                <ColumnList columns={def.lanes} />
                {def.exits && def.exits.length > 0 && <ColumnList columns={def.exits} className="text-muted-foreground" />}
              </div>
            )}
          </div>
          {diagram && diagram.orphanStages.length > 0 && (
            <p className="text-[11px] text-muted-foreground">
              No lane could be told for stage{diagram.orphanStages.length > 1 ? "s" : ""}{" "}
              <span className="font-mono">{diagram.orphanStages.join(", ")}</span>; their arrows start from the first lane.
            </p>
          )}
          {def.guide ? (
            <div className="prose prose-sm dark:prose-invert max-w-none prose-table:border prose-th:border prose-th:px-3 prose-th:py-1.5 prose-td:border prose-td:px-3 prose-td:py-1.5 prose-thead:bg-muted/50 break-words">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>{def.guide}</ReactMarkdown>
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">
              This pipeline has no guide yet. Ask an agent to write one — "add a guide to the {def.name} pipeline
              explaining each lane" — and it lands here with the definition.
            </p>
          )}
        </CardContent>
      )}
    </Card>
  );
}
