import { ChevronUp, ChevronDown, Trash2, Plus, Heading, Minus, Sigma } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ComponentsMultiSelect } from "./components-multi-select";
import { useConsolidationTemplateEditorStore } from "@/stores/consolidation-template-editor-store";
import type { TemplateLineKind } from "@/lib/api/consolidation-templates";

const KINDS: { value: TemplateLineKind; label: string; icon: typeof Heading }[] = [
  { value: "header", label: "Header", icon: Heading },
  { value: "line", label: "Line", icon: Minus },
  { value: "subtotal", label: "Subtotal", icon: Sigma },
];

/** Ordered, reorderable list of canonical statement lines. */
export function LinesEditor() {
  const template = useConsolidationTemplateEditorStore((s) => s.template);
  const addLine = useConsolidationTemplateEditorStore((s) => s.addLine);
  const removeLine = useConsolidationTemplateEditorStore((s) => s.removeLine);
  const moveLine = useConsolidationTemplateEditorStore((s) => s.moveLine);
  const setLineLabel = useConsolidationTemplateEditorStore((s) => s.setLineLabel);
  const setLineKind = useConsolidationTemplateEditorStore((s) => s.setLineKind);
  const setLineComponents = useConsolidationTemplateEditorStore((s) => s.setLineComponents);

  // Only `line`-kind labels can be summed by a subtotal.
  const lineLabels = template.lines
    .filter((l) => l.kind === "line" && l.label.trim())
    .map((l) => l.label);

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-sm font-semibold">Canonical Lines</h2>
          <p className="text-xs text-muted-foreground">
            The ordered statement layout. Subtotals sum selected lines.
          </p>
        </div>
        <div className="flex items-center gap-1.5">
          {KINDS.map(({ value, label, icon: Icon }) => (
            <Button key={value} variant="outline" size="sm" className="h-7 text-xs" onClick={() => addLine(value)}>
              <Plus className="h-3 w-3 mr-1" />
              <Icon className="h-3 w-3 mr-1" />
              {label}
            </Button>
          ))}
        </div>
      </div>

      {template.lines.length === 0 ? (
        <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
          No lines yet. Add a header, line, or subtotal — or upload a template.
        </div>
      ) : (
        <ul className="space-y-1.5">
          {template.lines.map((line, index) => (
            <li
              key={index}
              className="flex items-center gap-2 rounded-md border bg-card px-2 py-1.5"
            >
              <div className="flex flex-col">
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-5 w-5"
                  disabled={index === 0}
                  onClick={() => moveLine(index, -1)}
                  aria-label="Move up"
                >
                  <ChevronUp className="h-3.5 w-3.5" />
                </Button>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-5 w-5"
                  disabled={index === template.lines.length - 1}
                  onClick={() => moveLine(index, 1)}
                  aria-label="Move down"
                >
                  <ChevronDown className="h-3.5 w-3.5" />
                </Button>
              </div>

              <select
                className="h-8 rounded-md border bg-background px-2 text-xs shrink-0"
                value={line.kind}
                onChange={(e) => setLineKind(index, e.target.value as TemplateLineKind)}
              >
                {KINDS.map((k) => (
                  <option key={k.value} value={k.value}>
                    {k.label}
                  </option>
                ))}
              </select>

              <Input
                className={`h-8 text-sm ${line.kind === "header" ? "font-semibold" : line.kind === "subtotal" ? "font-medium" : ""}`}
                placeholder={
                  line.kind === "header"
                    ? "Section header"
                    : line.kind === "subtotal"
                      ? "Subtotal label"
                      : "Line label"
                }
                value={line.label}
                onChange={(e) => setLineLabel(index, e.target.value)}
              />

              {line.kind === "subtotal" && (
                <ComponentsMultiSelect
                  options={lineLabels}
                  selected={line.components}
                  onChange={(next) => setLineComponents(index, next)}
                />
              )}

              <Button
                variant="ghost"
                size="icon"
                className="h-7 w-7 shrink-0 text-destructive hover:text-destructive"
                onClick={() => removeLine(index)}
                aria-label="Remove line"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </Button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
