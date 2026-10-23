import { useDeferredValue, useMemo, useState } from "react";
import { X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Command,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
} from "@/components/ui/command";
import { useTools } from "@/hooks/use-tools";
import type { ToolInfo } from "@/lib/api/tools";
import { Combobox, ComboboxItemRow, type ComboboxOption } from "./combobox";

interface ToolGroup {
  /** Stable key + sort handle: `kind|label`. */
  key: string;
  kind: "builtin" | "mcp";
  label: string;
  /** Header text, e.g. `Vault (built-in)` / `Metabase (MCP)`. */
  heading: string;
  /** Shared `prefix.*` wildcard pattern, or null when tools share no prefix. */
  wildcard: string | null;
  tools: ToolInfo[];
}

/** Cap rendered tools per group so a huge MCP catalogue can't flood the DOM;
 * the user narrows with the search box, which is what keeps this fast. */
const MAX_TOOLS_PER_GROUP = 12;

/** Derive the integration/group a tool belongs to, falling back to its
 * name prefix when the API hasn't supplied provenance (mid-deploy / old API). */
function toolGroupKey(t: ToolInfo): { kind: "builtin" | "mcp"; label: string } {
  if (t.provenance) return { kind: t.provenance.kind, label: t.provenance.label };
  const dot = t.name.indexOf(".");
  const prefix = dot > 0 ? t.name.slice(0, dot) : t.name;
  return { kind: "builtin", label: prefix.charAt(0).toUpperCase() + prefix.slice(1) };
}

/** The dot-prefix shared by every tool in a group, or null when they differ
 * or any tool has no namespace (e.g. `ping`) — then `prefix.*` is meaningless. */
function sharedPrefix(tools: ToolInfo[]): string | null {
  let prefix: string | null = null;
  for (const t of tools) {
    const dot = t.name.indexOf(".");
    if (dot <= 0) return null;
    const p = t.name.slice(0, dot);
    if (prefix === null) prefix = p;
    else if (prefix !== p) return null;
  }
  return prefix;
}

/** The group's selectable wildcard. MCP tools are namespaced `mcp.<server>.<tool>`,
 * so scope to that server (`mcp.<server>.*`); the bare first-dot prefix would be
 * `mcp.*` and match every server. Built-ins use their shared dot-prefix. */
function groupWildcard(kind: "builtin" | "mcp", tools: ToolInfo[]): string | null {
  if (kind === "mcp") {
    const servers = new Set(
      tools.map((t) => t.provenance?.server).filter((s): s is string => !!s),
    );
    return servers.size === 1 ? `mcp.${[...servers][0]}.*` : null;
  }
  const prefix = sharedPrefix(tools);
  return prefix ? `${prefix}.*` : null;
}

function buildGroups(tools: ToolInfo[]): ToolGroup[] {
  const byKey = new Map<string, { kind: "builtin" | "mcp"; label: string; tools: ToolInfo[] }>();
  for (const t of tools) {
    const { kind, label } = toolGroupKey(t);
    const key = `${kind}|${label}`;
    const entry = byKey.get(key) ?? { kind, label, tools: [] };
    entry.tools.push(t);
    byKey.set(key, entry);
  }
  return [...byKey.values()]
    .sort((a, b) =>
      a.kind !== b.kind
        ? a.kind === "builtin"
          ? -1
          : 1
        : a.label.localeCompare(b.label),
    )
    .map(({ kind, label, tools }) => ({
      key: `${kind}|${label}`,
      kind,
      label,
      heading: `${label} (${kind === "mcp" ? "MCP" : "built-in"})`,
      wildcard: groupWildcard(kind, tools),
      tools: [...tools].sort((a, b) => a.name.localeCompare(b.name)),
    }));
}

interface FilteredGroup extends ToolGroup {
  /** Tools to render (capped); `hidden` is how many the cap dropped. */
  visible: ToolInfo[];
  hidden: number;
}

/** Substring-filter groups by the query (name + description + group label) and
 * cap each group's tool list. A group whose label/heading/wildcard matches keeps
 * all its tools, so typing an integration name surfaces the whole group. */
function filterGroups(groups: ToolGroup[], query: string): FilteredGroup[] {
  const q = query.trim().toLowerCase();
  const out: FilteredGroup[] = [];
  for (const g of groups) {
    const labelHit =
      !q ||
      g.label.toLowerCase().includes(q) ||
      g.heading.toLowerCase().includes(q) ||
      (g.wildcard?.toLowerCase().includes(q) ?? false);
    const matched = labelHit
      ? g.tools
      : g.tools.filter((t) =>
          `${t.name} ${t.description ?? ""}`.toLowerCase().includes(q),
        );
    if (matched.length === 0) continue;
    out.push({
      ...g,
      visible: matched.slice(0, MAX_TOOLS_PER_GROUP),
      hidden: Math.max(0, matched.length - MAX_TOOLS_PER_GROUP),
    });
  }
  return out;
}

/**
 * Multi-select tool catalogue. Tools are grouped by integration (built-in vs
 * MCP); each group leads with a selectable `prefix.*` wildcard. Filtering is done
 * here (controlled input + `useDeferredValue`) with cmdk's own filter disabled,
 * so only matching items render — fast even over a large MCP catalogue. A
 * free-text escape hatch covers patterns the catalogue can't see.
 */
export function ToolPicker({
  selected,
  onChange,
}: {
  selected: string[];
  onChange: (patterns: string[]) => void;
}) {
  const { data, isLoading, isError, error } = useTools();
  const tools = useMemo(() => data?.tools ?? [], [data]);
  const [custom, setCustom] = useState("");
  const [query, setQuery] = useState("");
  const deferredQuery = useDeferredValue(query);

  const groups = useMemo(() => buildGroups(tools), [tools]);
  const filtered = useMemo(
    () => filterGroups(groups, deferredQuery),
    [groups, deferredQuery],
  );

  const selectedSet = new Set(selected);
  const toggle = (pattern: string) =>
    onChange(
      selectedSet.has(pattern)
        ? selected.filter((p) => p !== pattern)
        : [...selected, pattern],
    );

  const addCustom = () => {
    const p = custom.trim();
    if (p && !selectedSet.has(p)) onChange([...selected, p]);
    setCustom("");
  };

  const empty = isLoading
    ? "Loading tools…"
    : isError
      ? `Couldn't load tools: ${(error as Error)?.message ?? "unknown error"}`
      : "No tools match.";

  return (
    <div className="space-y-2">
      {selected.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {selected.map((p) => (
            <Badge key={p} variant="secondary" className="gap-1 font-mono">
              {p}
              <button
                type="button"
                onClick={() => toggle(p)}
                aria-label={`Remove ${p}`}
                className="rounded-sm hover:text-destructive"
              >
                <X className="h-3 w-3" />
              </button>
            </Badge>
          ))}
        </div>
      )}

      <Command className="rounded-md border" shouldFilter={false} loop>
        <CommandInput
          placeholder="Search tools or integrations…"
          value={query}
          onValueChange={setQuery}
        />
        <CommandList>
          {filtered.length === 0 && <CommandEmpty>{empty}</CommandEmpty>}
          {filtered.map((g) => (
            <CommandGroup key={g.key} heading={g.heading}>
              {g.wildcard && (
                <CommandItem value={g.wildcard} onSelect={() => toggle(g.wildcard!)}>
                  <ComboboxItemRow
                    checked={selectedSet.has(g.wildcard)}
                    label={g.wildcard}
                    sub={`Whole group · ${g.tools.length} tool${g.tools.length === 1 ? "" : "s"}`}
                    mono
                  />
                </CommandItem>
              )}
              {g.visible.map((t) => (
                <CommandItem key={t.name} value={t.name} onSelect={() => toggle(t.name)}>
                  <ComboboxItemRow
                    checked={selectedSet.has(t.name)}
                    label={t.name}
                    sub={t.description || undefined}
                    mono
                  />
                </CommandItem>
              ))}
              {g.hidden > 0 && (
                <div className="px-3 py-1.5 text-xs text-muted-foreground">
                  +{g.hidden} more — refine your search
                </div>
              )}
            </CommandGroup>
          ))}
        </CommandList>
      </Command>

      <div className="flex gap-2">
        <Input
          value={custom}
          onChange={(e) => setCustom(e.target.value)}
          placeholder="Custom pattern, e.g. mcp.github.*"
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              addCustom();
            }
          }}
        />
        <Button type="button" variant="outline" onClick={addCustom} disabled={!custom.trim()}>
          Add
        </Button>
      </div>
    </div>
  );
}

/** Single-select tool picker — used by the effective-permission lookup. Same
 * controlled, prefiltered approach as [`ToolPicker`] so it stays responsive over
 * a large catalogue. */
export function ToolCombobox({
  value,
  onChange,
  id,
}: {
  value: string;
  onChange: (name: string) => void;
  id?: string;
}) {
  const { data, isLoading, isError } = useTools();
  const options: ComboboxOption[] = useMemo(
    () =>
      (data?.tools ?? []).map((t) => ({
        value: t.name,
        label: t.name,
        sub: t.description || undefined,
        mono: true,
      })),
    [data],
  );
  return (
    <Combobox
      options={options}
      value={value}
      onChange={(v) => onChange(v)}
      isLoading={isLoading}
      isError={isError}
      errorMessage="Couldn't load tools."
      placeholder="Select tool…"
      searchPlaceholder="Search tools…"
      emptyLabel="No tools match."
      id={id}
    />
  );
}
