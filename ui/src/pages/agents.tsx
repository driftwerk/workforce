import { useState, useMemo } from "react";
import { Bot, Search, Plus } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { AgentCard } from "@/components/agents/agent-card";
import { CreateAgentDialog } from "@/components/agents/create-agent-dialog";
import { useAgents } from "@/hooks/use-agents";

export default function AgentsPage() {
  const { data, isLoading, error } = useAgents();
  const [search, setSearch] = useState("");
  const [createOpen, setCreateOpen] = useState(false);

  const agents = data?.agents ?? [];

  const filtered = useMemo(() => {
    if (!search) return agents;
    const q = search.toLowerCase();
    return agents.filter(
      (a) =>
        (a.name ?? "").toLowerCase().includes(q) ||
        a.id.toLowerCase().includes(q),
    );
  }, [agents, search]);

  if (isLoading) return <PageSkeleton />;

  if (error) {
    return (
      <div className="p-6">
        <div className="text-destructive">
          Failed to load agents: {error instanceof Error ? error.message : "Unknown error"}
        </div>
      </div>
    );
  }

  return (
    <div className="p-6 space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold">Agents</h1>
          <p className="text-muted-foreground">Manage your AI agents.</p>
        </div>
        <Button onClick={() => setCreateOpen(true)}>
          <Plus className="h-4 w-4 mr-1" />
          Create Agent
        </Button>
      </div>

      {/* Search */}
      <div className="flex items-center gap-3 flex-wrap">
        <div className="relative flex-1 min-w-[200px] max-w-sm">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
          <Input
            placeholder="Search by name or ID..."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="pl-9"
          />
        </div>
      </div>

      {/* Grid or Empty */}
      {filtered.length === 0 ? (
        <EmptyState
          icon={Bot}
          title="No agents found"
          description={
            search
              ? "Try adjusting your search."
              : "Agents will appear here once they are configured."
          }
          action={
            !search
              ? { label: "Create Agent", onClick: () => setCreateOpen(true) }
              : undefined
          }
        />
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {filtered.map((agent) => (
            <AgentCard key={agent.id} agent={agent} linkTo={`/agents/${agent.id}`} />
          ))}
        </div>
      )}

      <CreateAgentDialog open={createOpen} onOpenChange={setCreateOpen} />
    </div>
  );
}
