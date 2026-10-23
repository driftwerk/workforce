import { ChevronRight } from "lucide-react";
import { SessionListTable } from "@/components/sessions/session-list-table";
import type { SessionGroup } from "@/lib/session-grouping";
import { cn } from "@/lib/utils";

/** Recursively render grouped sessions. Leaves use the same `SessionListTable`
 *  as the flat view, so row columns and click-through behaviour stay identical.
 *  Collapse state is owned by the caller (persisted in localStorage). */
export function SessionGroups({
  groups,
  collapsed,
  onToggle,
}: {
  groups: SessionGroup[];
  collapsed: Record<string, boolean>;
  onToggle: (key: string) => void;
}) {
  return (
    <div className="space-y-2">
      {groups.map((g) => {
        const isCollapsed = collapsed[g.key];
        return (
          <div key={g.key} className="border rounded-lg overflow-hidden">
            <button
              type="button"
              onClick={() => onToggle(g.key)}
              className="w-full flex items-center gap-2 p-3 bg-muted/50 hover:bg-muted text-left transition-colors"
            >
              <ChevronRight
                className={cn("h-4 w-4 shrink-0 transition-transform", !isCollapsed && "rotate-90")}
              />
              <span className="font-medium truncate">{g.title}</span>
              {g.subtitle && (
                <span className="text-xs text-muted-foreground truncate">{g.subtitle}</span>
              )}
              <span className="ml-auto text-xs text-muted-foreground tabular-nums">{g.count}</span>
            </button>
            {!isCollapsed && (
              <div className="p-2">
                {g.subgroups ? (
                  <SessionGroups groups={g.subgroups} collapsed={collapsed} onToggle={onToggle} />
                ) : (
                  <SessionListTable sessions={g.sessions} />
                )}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
