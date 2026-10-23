import { Fragment, useMemo, useState } from "react";
import {
  AlertTriangle,
  ArrowLeftRight,
  ChevronDown,
  ChevronRight,
  LayoutGrid,
  List,
  Pencil,
  Search,
  Trash2,
  UserPlus,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { TableSkeleton } from "@/components/loading-skeleton";
import { EmptyState } from "@/components/empty-state";
import { TierBadge, tierLabel } from "./tier-badge";
import { useVaultViewPrefs } from "@/hooks/use-vault-view-prefs";
import {
  VAULT_TIERS,
  makePathClassifier,
  type CatalogEntry,
  type PathClass,
  type VaultEntryView,
} from "@/lib/api/vault";

interface VaultTableProps {
  entries: VaultEntryView[];
  isLoading: boolean;
  /** Show the Owner column (admin views only). */
  showOwner: boolean;
  /** Predicate: can the active user move this entry? */
  canMove?: (entry: VaultEntryView) => boolean;
  onMove?: (entry: VaultEntryView) => void;
  /** Predicate: can the active user copy this entry to other users? */
  canCopy?: (entry: VaultEntryView) => boolean;
  onCopy?: (entry: VaultEntryView) => void;
  /** Predicate: can the active user rename (fix the path of) this entry? */
  canRename?: (entry: VaultEntryView) => boolean;
  onRename?: (entry: VaultEntryView) => void;
  /** Predicate: can the active user delete this entry? */
  canDelete?: (entry: VaultEntryView) => boolean;
  onDelete?: (entry: VaultEntryView) => void;
  /** Optional empty-state CTA when there are zero matching entries. */
  emptyAction?: { label: string; onClick: () => void };
  /** When provided, entries are classified against it for grouping + the
   * "unrecognised" badge (DW-538). */
  catalog?: CatalogEntry[];
}

interface ClassifiedEntry {
  entry: VaultEntryView;
  cls: PathClass;
}

interface ConfigGroup {
  configuration: string | null;
  rows: ClassifiedEntry[];
}

interface SkillGroup {
  group: string;
  total: number;
  configs: ConfigGroup[];
}

export function VaultTable({
  entries,
  isLoading,
  showOwner,
  canMove,
  onMove,
  canCopy,
  onCopy,
  canRename,
  onRename,
  canDelete,
  onDelete,
  emptyAction,
  catalog,
}: VaultTableProps) {
  const [prefs, setPrefs] = useVaultViewPrefs();
  const [search, setSearch] = useState("");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());

  const classify = useMemo(
    () => makePathClassifier(catalog ?? []),
    [catalog],
  );

  // Degrade a remembered tier filter that matches nothing here back to "all",
  // so a chip carried from another view never lands this one on an empty state.
  const tierFilter =
    prefs.tierFilter !== "all" &&
    entries.some((e) => e.tier === prefs.tierFilter)
      ? prefs.tierFilter
      : "all";

  const orphanCount = useMemo(
    () => (catalog ? entries.filter((e) => classify(e.path).orphan).length : 0),
    [entries, classify, catalog],
  );

  // Like the tier guard: a remembered "unrecognised" filter only bites where
  // there are orphans to show, so it can't strand a clean view at "no matches"
  // with its chip (rendered on the same condition) hidden.
  const onlyOrphans = prefs.onlyOrphans && orphanCount > 0;

  const filtered = useMemo(() => {
    const lower = search.toLowerCase();
    return entries.filter((e) => {
      if (tierFilter !== "all" && e.tier !== tierFilter) return false;
      if (onlyOrphans && !classify(e.path).orphan) return false;
      if (!lower) return true;
      return (
        e.path.toLowerCase().includes(lower) ||
        (e.created_by_display ?? "").toLowerCase().includes(lower)
      );
    });
  }, [entries, search, tierFilter, onlyOrphans, classify]);

  const groups = useMemo(
    () => buildGroups(filtered, classify),
    [filtered, classify],
  );

  const colSpan = showOwner ? 7 : 6;

  function toggleGroup(group: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(group)) next.delete(group);
      else next.add(group);
      return next;
    });
  }

  const rowProps = { showOwner, canMove, onMove, canCopy, onCopy, canRename, onRename, canDelete, onDelete };

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <Search className="absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
          <Input
            placeholder="Filter by path or owner…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="pl-9"
          />
        </div>
        <ViewToggle
          mode={prefs.viewMode}
          onChange={(viewMode) => setPrefs({ viewMode })}
        />
      </div>
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <TierFilterChip
          active={tierFilter === "all"}
          onClick={() => setPrefs({ tierFilter: "all" })}
          label={`All (${entries.length})`}
        />
        {VAULT_TIERS.map((t) => (
          <TierFilterChip
            key={t}
            active={tierFilter === t}
            onClick={() => setPrefs({ tierFilter: t })}
            label={`${tierLabel(t)} (${entries.filter((e) => e.tier === t).length})`}
          />
        ))}
        {catalog && orphanCount > 0 && (
          <button
            type="button"
            onClick={() => setPrefs({ onlyOrphans: !onlyOrphans })}
            className={
              "rounded-full border px-3 py-1 transition-colors inline-flex items-center gap-1 " +
              (onlyOrphans
                ? "border-amber-500 bg-amber-500 text-white"
                : "border-amber-500/50 text-amber-700 dark:text-amber-400 hover:bg-amber-500/10")
            }
            title="Secrets whose path matches no known skill/credential template — likely stale or orphaned"
          >
            <AlertTriangle className="h-3 w-3" /> Unrecognised ({orphanCount})
          </button>
        )}
      </div>

      {isLoading ? (
        <TableSkeleton rows={5} />
      ) : filtered.length === 0 ? (
        <EmptyState
          icon={Search}
          title={search || tierFilter !== "all" || onlyOrphans ? "No matches" : "Vault is empty"}
          description={
            search || tierFilter !== "all" || onlyOrphans
              ? "No entries match the current filter."
              : "Add a credential to get started."
          }
          action={emptyAction}
        />
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className={headCls}>Tier</th>
                <th className={headCls}>Path</th>
                <th className={headCls}>Agent</th>
                {showOwner && <th className={headCls}>Owner</th>}
                <th className={headCls}>Fingerprint</th>
                <th className={headCls}>Updated</th>
                <th className={headCls + " text-right"}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {prefs.viewMode === "flat"
                ? filtered.map((entry) => (
                    <VaultRow
                      key={rowKey(entry)}
                      entry={entry}
                      cls={classify(entry.path)}
                      {...rowProps}
                    />
                  ))
                : groups.map((g) => (
                    <Fragment key={g.group}>
                      <tr
                        className="border-b bg-muted/30 cursor-pointer hover:bg-muted/50"
                        onClick={() => toggleGroup(g.group)}
                      >
                        <td colSpan={colSpan} className="px-3 py-1.5">
                          <span className="inline-flex items-center gap-1.5 text-sm font-medium">
                            {collapsed.has(g.group) ? (
                              <ChevronRight className="h-3.5 w-3.5" />
                            ) : (
                              <ChevronDown className="h-3.5 w-3.5" />
                            )}
                            {g.group}
                            <span className="text-xs font-normal text-muted-foreground">
                              ({g.total})
                            </span>
                          </span>
                        </td>
                      </tr>
                      {!collapsed.has(g.group) &&
                        g.configs.map((cfg) => (
                          <Fragment key={cfg.configuration ?? "—"}>
                            {cfg.configuration != null && (
                              <tr className="border-b bg-muted/10">
                                <td
                                  colSpan={colSpan}
                                  className="px-3 py-1 pl-9 text-xs font-mono text-muted-foreground"
                                >
                                  {cfg.configuration}
                                </td>
                              </tr>
                            )}
                            {cfg.rows.map(({ entry, cls }) => (
                              <VaultRow
                                key={rowKey(entry)}
                                entry={entry}
                                cls={cls}
                                {...rowProps}
                              />
                            ))}
                          </Fragment>
                        ))}
                    </Fragment>
                  ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function buildGroups(
  entries: VaultEntryView[],
  classify: (path: string) => PathClass,
): SkillGroup[] {
  const byGroup = new Map<string, ClassifiedEntry[]>();
  for (const entry of entries) {
    const cls = classify(entry.path);
    const arr = byGroup.get(cls.group) ?? [];
    arr.push({ entry, cls });
    byGroup.set(cls.group, arr);
  }

  const groups: SkillGroup[] = [...byGroup.entries()].map(([group, rows]) => {
    const byConfig = new Map<string | null, ClassifiedEntry[]>();
    for (const row of rows) {
      const key = row.cls.configuration;
      const arr = byConfig.get(key) ?? [];
      arr.push(row);
      byConfig.set(key, arr);
    }
    const configs: ConfigGroup[] = [...byConfig.entries()]
      .map(([configuration, configRows]) => ({
        configuration,
        rows: configRows.sort((a, b) => a.entry.path.localeCompare(b.entry.path)),
      }))
      // Bare (no-configuration) rows first, then configurations alphabetically.
      .sort((a, b) => {
        if (a.configuration == null) return -1;
        if (b.configuration == null) return 1;
        return a.configuration.localeCompare(b.configuration);
      });
    return { group, total: rows.length, configs };
  });

  // Skills alphabetically, with the catch-all "Other" group last.
  return groups.sort((a, b) => {
    if (a.group === "Other") return 1;
    if (b.group === "Other") return -1;
    return a.group.localeCompare(b.group);
  });
}

function rowKey(entry: VaultEntryView): string {
  return `${entry.tier}:${entry.agent_id ?? "-"}:${entry.path}:${entry.created_by}`;
}

interface VaultRowProps {
  entry: VaultEntryView;
  cls: PathClass;
  showOwner: boolean;
  canMove?: (entry: VaultEntryView) => boolean;
  onMove?: (entry: VaultEntryView) => void;
  canCopy?: (entry: VaultEntryView) => boolean;
  onCopy?: (entry: VaultEntryView) => void;
  canRename?: (entry: VaultEntryView) => boolean;
  onRename?: (entry: VaultEntryView) => void;
  canDelete?: (entry: VaultEntryView) => boolean;
  onDelete?: (entry: VaultEntryView) => void;
}

function VaultRow({
  entry,
  cls,
  showOwner,
  canMove,
  onMove,
  canCopy,
  onCopy,
  canRename,
  onRename,
  canDelete,
  onDelete,
}: VaultRowProps) {
  return (
    <tr className="border-b last:border-b-0 hover:bg-muted/20 transition-colors">
      <td className="px-4 py-2">
        <TierBadge tier={entry.tier} />
      </td>
      <td className="px-4 py-2">
        <span className="text-sm font-mono">{entry.path}</span>
        <SourceTag cls={cls} />
        {cls.orphan && (
          <span
            className="ml-2 inline-flex items-center gap-1 rounded-full border border-amber-500/50 px-1.5 py-0.5 text-[10px] text-amber-700 dark:text-amber-400 align-middle"
            title="No known skill/credential uses this path — likely stale or mis-pathed. Use Rename to move it to the correct path."
          >
            <AlertTriangle className="h-2.5 w-2.5" /> unrecognised
          </span>
        )}
        {cls.sharedOnly && entry.tier.endsWith("per_user") && (
          <span
            className="ml-2 inline-flex items-center gap-1 rounded-full border border-amber-500/50 px-1.5 py-0.5 text-[10px] text-amber-700 dark:text-amber-400 align-middle"
            title="This setting is shared configuration and is read from shared tiers only — a per-user copy is ignored. Move it to a shared tier or delete it."
          >
            <AlertTriangle className="h-2.5 w-2.5" /> shared-only · ignored here
          </span>
        )}
      </td>
      <td className="px-4 py-2 text-sm text-muted-foreground">
        {entry.agent_id ?? "—"}
      </td>
      {showOwner && (
        <td className="px-4 py-2">
          <span className="text-sm">{entry.created_by_display ?? entry.created_by}</span>
        </td>
      )}
      <td className="px-4 py-2">
        <span className="text-xs font-mono text-muted-foreground">{entry.fingerprint}</span>
      </td>
      <td className="px-4 py-2 text-xs text-muted-foreground">
        {new Date(entry.updated_at).toLocaleDateString()}
      </td>
      <td className="px-4 py-2 text-right space-x-1">
        {canRename?.(entry) && onRename && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 px-2 text-xs"
            onClick={() => onRename(entry)}
            title="Rename / fix the path"
          >
            <Pencil className="h-3.5 w-3.5 mr-1" /> Rename
          </Button>
        )}
        {canMove?.(entry) && onMove && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 px-2 text-xs"
            onClick={() => onMove(entry)}
            title="Move to a different tier"
          >
            <ArrowLeftRight className="h-3.5 w-3.5 mr-1" /> Move
          </Button>
        )}
        {canCopy?.(entry) && onCopy && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 px-2 text-xs"
            onClick={() => onCopy(entry)}
            title="Copy to other users"
          >
            <UserPlus className="h-3.5 w-3.5 mr-1" /> Copy
          </Button>
        )}
        {canDelete?.(entry) && onDelete && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 px-2 text-xs text-destructive hover:text-destructive"
            onClick={() => onDelete(entry)}
          >
            <Trash2 className="h-3.5 w-3.5 mr-1" /> Delete
          </Button>
        )}
      </td>
    </tr>
  );
}

/** Built-in vs MCP origin (and OAuth-managed flag) for a recognised path. */
function SourceTag({ cls }: { cls: PathClass }) {
  if (!cls.source) return null;
  const label = cls.source === "mcp" ? "MCP" : "built-in";
  return (
    <span className="ml-2 inline-flex items-center rounded-full border border-muted px-1.5 py-0.5 text-[10px] text-muted-foreground align-middle">
      {label}
      {cls.managed_by === "oauth" ? " · OAuth" : ""}
    </span>
  );
}

const headCls = "text-left text-xs font-medium text-muted-foreground px-4 py-3";

function ViewToggle({
  mode,
  onChange,
}: {
  mode: "grouped" | "flat";
  onChange: (mode: "grouped" | "flat") => void;
}) {
  return (
    <div className="inline-flex rounded-md border overflow-hidden text-xs">
      <button
        type="button"
        onClick={() => onChange("grouped")}
        className={
          "inline-flex items-center gap-1 px-2.5 py-2 transition-colors " +
          (mode === "grouped"
            ? "bg-foreground text-background"
            : "hover:bg-muted/40")
        }
        title="Group by skill / configuration"
      >
        <LayoutGrid className="h-3.5 w-3.5" /> Grouped
      </button>
      <button
        type="button"
        onClick={() => onChange("flat")}
        className={
          "inline-flex items-center gap-1 px-2.5 py-2 transition-colors border-l " +
          (mode === "flat" ? "bg-foreground text-background" : "hover:bg-muted/40")
        }
        title="Flat list"
      >
        <List className="h-3.5 w-3.5" /> List
      </button>
    </div>
  );
}

function TierFilterChip({
  active,
  onClick,
  label,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={
        "rounded-full border px-3 py-1 transition-colors " +
        (active
          ? "border-foreground bg-foreground text-background"
          : "border-muted hover:bg-muted/40")
      }
    >
      {label}
    </button>
  );
}
