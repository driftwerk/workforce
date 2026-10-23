import { useState, useMemo } from "react";
import { FileText, Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { SessionListTable } from "@/components/sessions/session-list-table";
import { SessionGroups } from "@/components/sessions/session-groups";
import { useSessions } from "@/hooks/use-sessions";
import { useAgents } from "@/hooks/use-agents";
import {
  useSessionsViewPrefs,
  type SessionViewMode,
} from "@/hooks/use-sessions-view-prefs";
import { groupByAgent, groupByUser } from "@/lib/session-grouping";

type CategoryFilter = "all" | "user" | "system";

const CATEGORY_OPTIONS: { value: CategoryFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "user", label: "User" },
  { value: "system", label: "System" },
];

const VIEW_OPTIONS: { value: SessionViewMode; label: string }[] = [
  { value: "flat", label: "Flat" },
  { value: "user", label: "By user" },
  { value: "agent", label: "By agent" },
];

export default function SessionsPage() {
  const [agentFilter, setAgentFilter] = useState<string>("");
  const [search, setSearch] = useState("");
  const [category, setCategory] = useState<CategoryFilter>("all");
  const { prefs, update, toggleCollapsed } = useSessionsViewPrefs();

  const { data, isLoading, error } = useSessions(agentFilter || undefined);
  const { data: agentData } = useAgents();

  const agents = agentData?.agents ?? [];
  const sessions = data?.sessions ?? [];

  const filtered = useMemo(() => {
    const q = search.toLowerCase();
    return sessions.filter((s) => {
      if (category !== "all" && s.session_category !== category) return false;
      if (!q) return true;
      return (
        (s.label ?? "").toLowerCase().includes(q) ||
        s.id.toLowerCase().includes(q) ||
        s.channel.toLowerCase().includes(q) ||
        (s.channel_display?.name ?? "").toLowerCase().includes(q) ||
        s.owner.toLowerCase().includes(q) ||
        s.owner_display_name.toLowerCase().includes(q) ||
        s.agent_display_name.toLowerCase().includes(q)
      );
    });
  }, [sessions, search, category]);

  const groups = useMemo(() => {
    if (prefs.viewMode === "user") return groupByUser(filtered);
    if (prefs.viewMode === "agent") return groupByAgent(filtered, prefs.agentSubgroupByUser);
    return [];
  }, [filtered, prefs.viewMode, prefs.agentSubgroupByUser]);

  if (isLoading) return <PageSkeleton />;

  if (error) {
    return (
      <div className="p-6">
        <div className="text-destructive">
          Failed to load sessions: {error instanceof Error ? error.message : "Unknown error"}
        </div>
      </div>
    );
  }

  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-2xl font-bold">Sessions</h1>
        <p className="text-muted-foreground">
          Every agent session — user-driven conversations and system tasks alike.
        </p>
      </div>

      <div className="flex items-center gap-3 flex-wrap">
        <div className="relative flex-1 min-w-[200px] max-w-sm">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
          <Input
            placeholder="Search label, ID, channel, user, or agent..."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="pl-9"
          />
        </div>

        <div className="flex rounded-md border overflow-hidden">
          {CATEGORY_OPTIONS.map((opt) => (
            <button
              key={opt.value}
              onClick={() => setCategory(opt.value)}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                category === opt.value
                  ? "bg-primary text-primary-foreground"
                  : "bg-background hover:bg-muted text-muted-foreground"
              }`}
            >
              {opt.label}
            </button>
          ))}
        </div>

        <div className="flex rounded-md border overflow-hidden">
          {VIEW_OPTIONS.map((opt) => (
            <button
              key={opt.value}
              onClick={() => update({ viewMode: opt.value })}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${
                prefs.viewMode === opt.value
                  ? "bg-primary text-primary-foreground"
                  : "bg-background hover:bg-muted text-muted-foreground"
              }`}
            >
              {opt.label}
            </button>
          ))}
        </div>

        {prefs.viewMode === "agent" && (
          <label className="flex items-center gap-1.5 text-xs text-muted-foreground cursor-pointer select-none">
            <input
              type="checkbox"
              checked={prefs.agentSubgroupByUser}
              onChange={(e) => update({ agentSubgroupByUser: e.target.checked })}
              className="h-3.5 w-3.5"
            />
            Sub-group by user
          </label>
        )}

        <select
          value={agentFilter}
          onChange={(e) => setAgentFilter(e.target.value)}
          className="h-10 rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <option value="">All agents</option>
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name ?? a.id}
            </option>
          ))}
        </select>

        <span className="text-sm text-muted-foreground ml-auto">
          {filtered.length.toLocaleString()} of {sessions.length.toLocaleString()}
        </span>
      </div>

      {filtered.length === 0 ? (
        <EmptyState
          icon={FileText}
          title="No sessions found"
          description={
            search || agentFilter || category !== "all"
              ? "Try adjusting your filters."
              : "Sessions will appear here once agents start running."
          }
        />
      ) : prefs.viewMode === "flat" ? (
        <SessionListTable sessions={filtered} />
      ) : (
        <SessionGroups
          groups={groups}
          collapsed={prefs.collapsed}
          onToggle={toggleCollapsed}
        />
      )}
    </div>
  );
}
