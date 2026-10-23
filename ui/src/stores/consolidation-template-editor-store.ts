import { create } from "zustand";
import type {
  ConsolidationTemplate,
  TemplateLine,
  TemplateLineKind,
} from "@/lib/api/consolidation-templates";

/** Canonical JSON serialisation — also the dirty-tracking baseline. */
export function serializeTemplate(t: ConsolidationTemplate): string {
  return JSON.stringify(t, null, 2);
}

function emptyTemplate(): ConsolidationTemplate {
  return { name: "", lines: [], map: {} };
}

/** Normalise a parsed template object, tolerating partial/missing fields. */
function normalizeTemplate(obj: unknown): ConsolidationTemplate {
  const t = (obj ?? {}) as Partial<ConsolidationTemplate>;
  const lines: TemplateLine[] = Array.isArray(t.lines)
    ? t.lines.map((l) => ({
        label: String(l?.label ?? ""),
        kind: (["header", "line", "subtotal"] as TemplateLineKind[]).includes(
          l?.kind as TemplateLineKind,
        )
          ? (l!.kind as TemplateLineKind)
          : "line",
        components: Array.isArray(l?.components)
          ? l!.components.map(String)
          : [],
      }))
    : [];
  const map: Record<string, Record<string, string>> = {};
  if (t.map && typeof t.map === "object") {
    for (const [entity, sources] of Object.entries(t.map)) {
      if (sources && typeof sources === "object") {
        map[entity] = {};
        for (const [src, canonical] of Object.entries(sources)) {
          map[entity][src] = String(canonical);
        }
      }
    }
  }
  return { name: t.name ?? "", lines, map };
}

/** Parse a stored template, tolerating partial/missing fields. */
export function parseTemplate(raw: string): ConsolidationTemplate {
  try {
    return normalizeTemplate(JSON.parse(raw));
  } catch {
    return emptyTemplate();
  }
}

interface EditorState {
  template: ConsolidationTemplate;
  /** JSON of the last-saved state; the dirty baseline. */
  savedJson: string;
  isDirty: boolean;

  load: (raw: string) => void;
  /** Replace the whole template (e.g. after parse-xlsx) and mark dirty. */
  replace: (t: ConsolidationTemplate, markDirty?: boolean) => void;
  markSaved: () => void;
  reset: () => void;

  setName: (name: string) => void;

  // --- Lines ---
  addLine: (kind: TemplateLineKind) => void;
  removeLine: (index: number) => void;
  moveLine: (index: number, dir: -1 | 1) => void;
  setLineLabel: (index: number, label: string) => void;
  setLineKind: (index: number, kind: TemplateLineKind) => void;
  setLineComponents: (index: number, components: string[]) => void;

  // --- Mapping (entity → { source → canonical }) ---
  addEntity: (entity: string) => void;
  removeEntity: (entity: string) => void;
  /** Add a source-account label under `entity` that maps to `canonical`. */
  addSource: (entity: string, canonical: string, source: string) => void;
  /** Remove a single source-account label under `entity`. */
  removeSource: (entity: string, source: string) => void;
  /** Rename a source label (delete old key, set new), keeping the canonical. */
  renameSource: (
    entity: string,
    oldSource: string,
    newSource: string,
    canonical: string,
  ) => void;
}

const INITIAL = {
  template: emptyTemplate(),
  savedJson: serializeTemplate(emptyTemplate()),
  isDirty: false,
};

/** Recompute `isDirty` against the saved baseline after a mutation. */
function withDirty(
  set: (partial: Partial<EditorState>) => void,
  template: ConsolidationTemplate,
  savedJson: string,
) {
  set({ template, isDirty: serializeTemplate(template) !== savedJson });
}

export const useConsolidationTemplateEditorStore = create<EditorState>(
  (set, get) => ({
    ...INITIAL,

    load: (raw: string) => {
      const template = parseTemplate(raw);
      set({ template, savedJson: serializeTemplate(template), isDirty: false });
    },

    replace: (t: ConsolidationTemplate, markDirty = true) => {
      const { savedJson } = get();
      const template = normalizeTemplate(t);
      if (markDirty) {
        withDirty(set, template, savedJson);
      } else {
        set({
          template,
          savedJson: serializeTemplate(template),
          isDirty: false,
        });
      }
    },

    markSaved: () => {
      const { template } = get();
      set({ savedJson: serializeTemplate(template), isDirty: false });
    },

    reset: () => set({ ...INITIAL }),

    setName: (name: string) => {
      const { template, savedJson } = get();
      withDirty(set, { ...template, name }, savedJson);
    },

    addLine: (kind: TemplateLineKind) => {
      const { template, savedJson } = get();
      const lines = [...template.lines, { label: "", kind, components: [] }];
      withDirty(set, { ...template, lines }, savedJson);
    },

    removeLine: (index: number) => {
      const { template, savedJson } = get();
      const removed = template.lines[index];
      if (!removed) return;
      const lines = template.lines.filter((_, i) => i !== index);
      // Purge references to the removed line from subtotal components and the
      // per-entity map (entries whose canonical target was this line).
      const label = removed.label;
      const cleanedLines = lines.map((l) =>
        l.kind === "subtotal"
          ? { ...l, components: l.components.filter((c) => c !== label) }
          : l,
      );
      const map = pruneMapForCanonical(template.map, label);
      withDirty(set, { ...template, lines: cleanedLines, map }, savedJson);
    },

    moveLine: (index: number, dir: -1 | 1) => {
      const { template, savedJson } = get();
      const target = index + dir;
      if (target < 0 || target >= template.lines.length) return;
      const lines = [...template.lines];
      const [moved] = lines.splice(index, 1);
      lines.splice(target, 0, moved!);
      withDirty(set, { ...template, lines }, savedJson);
    },

    setLineLabel: (index: number, label: string) => {
      const { template, savedJson } = get();
      const old = template.lines[index];
      if (!old) return;
      const oldLabel = old.label;
      const lines = template.lines.map((l, i) =>
        i === index ? { ...l, label } : l,
      );
      let map = template.map;
      let nextLines = lines;
      // Renaming a canonical line must rewrite every reference: subtotal
      // components and every map value pointing at the old label.
      if (oldLabel && oldLabel !== label) {
        nextLines = lines.map((l) =>
          l.kind === "subtotal"
            ? {
                ...l,
                components: l.components.map((c) =>
                  c === oldLabel ? label : c,
                ),
              }
            : l,
        );
        map = renameMapCanonical(template.map, oldLabel, label);
      }
      withDirty(set, { ...template, lines: nextLines, map }, savedJson);
    },

    setLineKind: (index: number, kind: TemplateLineKind) => {
      const { template, savedJson } = get();
      const old = template.lines[index];
      if (!old) return;
      const lines = template.lines.map((l, i) =>
        i === index
          ? { ...l, kind, components: kind === "subtotal" ? l.components : [] }
          : l,
      );
      // A non-`line` row can't be a mapping target; drop any map entries for it.
      const map =
        kind === "line"
          ? template.map
          : pruneMapForCanonical(template.map, old.label);
      withDirty(set, { ...template, lines, map }, savedJson);
    },

    setLineComponents: (index: number, components: string[]) => {
      const { template, savedJson } = get();
      const lines = template.lines.map((l, i) =>
        i === index ? { ...l, components } : l,
      );
      withDirty(set, { ...template, lines }, savedJson);
    },

    addEntity: (entity: string) => {
      const { template, savedJson } = get();
      if (!entity || template.map[entity]) return;
      const map = { ...template.map, [entity]: {} };
      withDirty(set, { ...template, map }, savedJson);
    },

    removeEntity: (entity: string) => {
      const { template, savedJson } = get();
      if (!template.map[entity]) return;
      const map = { ...template.map };
      delete map[entity];
      withDirty(set, { ...template, map }, savedJson);
    },

    addSource: (entity: string, canonical: string, source: string) => {
      const { template, savedJson } = get();
      const src = source.trim();
      if (!src) return;
      const map = {
        ...template.map,
        [entity]: { ...(template.map[entity] ?? {}), [src]: canonical },
      };
      withDirty(set, { ...template, map }, savedJson);
    },

    removeSource: (entity: string, source: string) => {
      const { template, savedJson } = get();
      const entry = template.map[entity];
      if (!entry || !(source in entry)) return;
      const next = { ...entry };
      delete next[source];
      const map = { ...template.map, [entity]: next };
      withDirty(set, { ...template, map }, savedJson);
    },

    renameSource: (
      entity: string,
      oldSource: string,
      newSource: string,
      canonical: string,
    ) => {
      const { template, savedJson } = get();
      const src = newSource.trim();
      // Rebuild preserving key order — replacing the key in place — so a
      // multi-source cell doesn't reshuffle (and steal focus) on rename.
      const entry: Record<string, string> = {};
      for (const [k, v] of Object.entries(template.map[entity] ?? {})) {
        if (k === oldSource) {
          if (src) entry[src] = canonical;
        } else {
          entry[k] = v;
        }
      }
      const map = { ...template.map, [entity]: entry };
      withDirty(set, { ...template, map }, savedJson);
    },
  }),
);

/** Drop every `source → canonical` entry whose canonical target is `label`. */
function pruneMapForCanonical(
  map: Record<string, Record<string, string>>,
  label: string,
): Record<string, Record<string, string>> {
  const out: Record<string, Record<string, string>> = {};
  for (const [entity, sources] of Object.entries(map)) {
    out[entity] = Object.fromEntries(
      Object.entries(sources).filter(([, canonical]) => canonical !== label),
    );
  }
  return out;
}

/** Rewrite every canonical value `from → to`, keeping source keys. */
function renameMapCanonical(
  map: Record<string, Record<string, string>>,
  from: string,
  to: string,
): Record<string, Record<string, string>> {
  const out: Record<string, Record<string, string>> = {};
  for (const [entity, sources] of Object.entries(map)) {
    out[entity] = Object.fromEntries(
      Object.entries(sources).map(([src, canonical]) => [
        src,
        canonical === from ? to : canonical,
      ]),
    );
  }
  return out;
}

/** All source labels under `entity` that map to canonical `label`. */
export function sourcesForCanonical(
  template: ConsolidationTemplate,
  entity: string,
  label: string,
): string[] {
  const entry = template.map[entity];
  if (!entry) return [];
  return Object.entries(entry)
    .filter(([, canonical]) => canonical === label)
    .map(([src]) => src);
}
