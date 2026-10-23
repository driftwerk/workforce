import { useEffect, useState } from "react";
import { Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  useConsolidationTemplateEditorStore,
  sourcesForCanonical,
} from "@/stores/consolidation-template-editor-store";

interface Props {
  /** Entity names from the Xero skill config (drives the columns). */
  entities: string[];
}

/**
 * Mapping grid: rows are canonical `line` labels, columns are entities. Each
 * cell holds the source-account labels (one or more) that roll up into that
 * canonical line for that entity, i.e. `map[entity][source] = canonical`.
 */
export function MappingGrid({ entities }: Props) {
  const template = useConsolidationTemplateEditorStore((s) => s.template);
  const addEntity = useConsolidationTemplateEditorStore((s) => s.addEntity);
  const removeEntity = useConsolidationTemplateEditorStore((s) => s.removeEntity);

  const [newEntity, setNewEntity] = useState("");

  // Union of configured entities and any already present in the map (so an
  // imported template's entities show even if the skill config lost them).
  const columns = Array.from(
    new Set([...entities, ...Object.keys(template.map)]),
  );

  // Keep the original index for a stable React key; map lookups still go by
  // label (two lines sharing a label is a user error the preview surfaces).
  const lineRows = template.lines
    .map((line, index) => ({ line, index }))
    .filter(({ line }) => line.kind === "line" && line.label.trim());

  const handleAddEntity = () => {
    const name = newEntity.trim();
    if (!name) return;
    addEntity(name);
    setNewEntity("");
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-2 flex-wrap">
        <div>
          <h2 className="text-sm font-semibold">Account Mapping</h2>
          <p className="text-xs text-muted-foreground">
            For each entity, the source account labels that roll up into each
            canonical line.
          </p>
        </div>
        <div className="flex items-center gap-1.5">
          <Input
            className="h-8 w-44 text-sm"
            placeholder="Add entity column…"
            value={newEntity}
            onChange={(e) => setNewEntity(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                handleAddEntity();
              }
            }}
          />
          <Button variant="outline" size="sm" className="h-8 text-xs" onClick={handleAddEntity}>
            <Plus className="h-3.5 w-3.5 mr-1" />
            Add
          </Button>
        </div>
      </div>

      {lineRows.length === 0 ? (
        <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
          Add some <span className="font-medium">Line</span> rows above to map
          accounts into them.
        </div>
      ) : columns.length === 0 ? (
        <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
          No entities configured. Add an entity column above to start mapping.
        </div>
      ) : (
        <div className="overflow-x-auto rounded-md border">
          <table className="w-full border-collapse text-sm">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className="sticky left-0 z-10 bg-muted/50 px-3 py-2 text-left font-medium min-w-[12rem]">
                  Canonical line
                </th>
                {columns.map((entity) => (
                  <th key={entity} className="px-3 py-2 text-left font-medium min-w-[14rem]">
                    <div className="flex items-center justify-between gap-2">
                      <span className="font-mono truncate">{entity}</span>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-5 w-5 text-muted-foreground hover:text-destructive"
                        onClick={() => removeEntity(entity)}
                        aria-label={`Remove ${entity}`}
                      >
                        <X className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {lineRows.map(({ line, index }) => (
                <tr key={index} className="border-b last:border-0 align-top">
                  <td className="sticky left-0 z-10 bg-card px-3 py-2 font-medium">
                    {line.label}
                  </td>
                  {columns.map((entity) => (
                    <td key={entity} className="px-3 py-2">
                      <MappingCell entity={entity} canonical={line.label} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function MappingCell({
  entity,
  canonical,
}: {
  entity: string;
  canonical: string;
}) {
  const template = useConsolidationTemplateEditorStore((s) => s.template);
  const addSource = useConsolidationTemplateEditorStore((s) => s.addSource);
  const removeSource = useConsolidationTemplateEditorStore((s) => s.removeSource);
  const renameSource = useConsolidationTemplateEditorStore((s) => s.renameSource);

  const [draft, setDraft] = useState("");
  const sources = sourcesForCanonical(template, entity, canonical);

  const commitAdd = () => {
    if (!draft.trim()) return;
    addSource(entity, canonical, draft);
    setDraft("");
  };

  return (
    <div className="space-y-1.5">
      {sources.map((src, i) => (
        <SourceRow
          // Index key: the map only mutates on commit (not per keystroke), so
          // row order is stable while editing — keeps the input from remounting
          // and losing focus mid-type.
          key={i}
          value={src}
          onRename={(next) => renameSource(entity, src, next, canonical)}
          onRemove={() => removeSource(entity, src)}
        />
      ))}
      <div className="flex items-center gap-1">
        <Input
          className="h-7 text-xs"
          placeholder="+ source account"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              commitAdd();
            }
          }}
          onBlur={commitAdd}
        />
      </div>
    </div>
  );
}

/**
 * One source-account input. Edits a local draft and commits on blur/Enter so
 * renaming the map key (which reorders it) doesn't remount the input mid-type.
 */
function SourceRow({
  value,
  onRename,
  onRemove,
}: {
  value: string;
  onRename: (next: string) => void;
  onRemove: () => void;
}) {
  const [draft, setDraft] = useState(value);
  // Resync when the committed value changes from elsewhere (not while focused).
  useEffect(() => setDraft(value), [value]);

  const commit = () => {
    const next = draft.trim();
    if (!next) {
      onRemove();
    } else if (next !== value) {
      onRename(next);
    }
  };

  return (
    <div className="flex items-center gap-1">
      <Input
        className="h-7 text-xs"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            commit();
          }
        }}
      />
      <Button
        variant="ghost"
        size="icon"
        className="h-6 w-6 shrink-0 text-muted-foreground hover:text-destructive"
        onClick={onRemove}
        aria-label="Remove source"
      >
        <X className="h-3 w-3" />
      </Button>
    </div>
  );
}
