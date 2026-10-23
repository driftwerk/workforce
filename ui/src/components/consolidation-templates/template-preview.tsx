import { useConsolidationTemplateEditorStore, sourcesForCanonical } from "@/stores/consolidation-template-editor-store";

interface Props {
  /** Entity columns to preview (config ∪ mapped). */
  entities: string[];
}

/**
 * Client-side preview of the canonical statement: lines in order, each line
 * showing the source accounts mapped to it per entity, subtotals showing the
 * lines they sum. Pure derived render — no backend.
 */
export function TemplatePreview({ entities }: Props) {
  const template = useConsolidationTemplateEditorStore((s) => s.template);

  const columns = Array.from(
    new Set([...entities, ...Object.keys(template.map)]),
  );

  if (template.lines.length === 0) {
    return (
      <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
        Nothing to preview yet.
      </div>
    );
  }

  return (
    <div className="overflow-x-auto rounded-md border bg-card">
      <table className="w-full border-collapse text-sm">
        <thead>
          <tr className="border-b bg-muted/50">
            <th className="px-3 py-2 text-left font-medium min-w-[14rem]">
              {template.name?.trim() || "Statement"}
            </th>
            {columns.map((entity) => (
              <th key={entity} className="px-3 py-2 text-left font-mono text-xs font-medium min-w-[12rem]">
                {entity}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {template.lines.map((line, index) => {
            if (line.kind === "header") {
              return (
                <tr key={index} className="border-b bg-muted/30">
                  <td
                    className="px-3 pt-3 pb-1 font-semibold uppercase tracking-wide text-xs"
                    colSpan={columns.length + 1}
                  >
                    {line.label || <span className="text-muted-foreground">(header)</span>}
                  </td>
                </tr>
              );
            }

            if (line.kind === "subtotal") {
              return (
                <tr key={index} className="border-b border-t-2 border-t-border font-medium">
                  <td className="px-3 py-2">
                    {line.label || <span className="text-muted-foreground">(subtotal)</span>}
                    {line.components.length > 0 && (
                      <span className="ml-2 text-xs font-normal text-muted-foreground">
                        = {line.components.join(" + ")}
                      </span>
                    )}
                  </td>
                  {columns.map((entity) => (
                    <td key={entity} className="px-3 py-2 text-muted-foreground">
                      —
                    </td>
                  ))}
                </tr>
              );
            }

            // kind === "line"
            return (
              <tr key={index} className="border-b last:border-0 align-top">
                <td className="px-3 py-2 pl-5">
                  {line.label || <span className="text-muted-foreground">(line)</span>}
                </td>
                {columns.map((entity) => {
                  const sources = line.label
                    ? sourcesForCanonical(template, entity, line.label)
                    : [];
                  return (
                    <td key={entity} className="px-3 py-2 text-xs">
                      {sources.length === 0 ? (
                        <span className="text-muted-foreground/50">unmapped</span>
                      ) : (
                        <ul className="space-y-0.5">
                          {sources.map((src) => (
                            <li key={src} className="text-muted-foreground">
                              {src}
                            </li>
                          ))}
                        </ul>
                      )}
                    </td>
                  );
                })}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
