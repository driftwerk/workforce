import { useState, useMemo } from "react";
import { Link } from "react-router";
import { Users, Search, Plus, Loader2 } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useIdentityTeams, useCreateIdentityTeam } from "@/hooks/use-teams";
import { useAuthStore } from "@/stores/auth-store";

export default function TeamsPage() {
  const { data, isLoading, error } = useIdentityTeams();
  const [search, setSearch] = useState("");

  const teams = data?.teams ?? [];

  const filtered = useMemo(() => {
    if (!search) return teams;
    const q = search.toLowerCase();
    return teams.filter((t) => t.name.toLowerCase().includes(q));
  }, [teams, search]);

  if (isLoading) return <PageSkeleton />;

  if (error) {
    return (
      <div className="p-6">
        <div className="text-destructive">
          Failed to load teams:{" "}
          {error instanceof Error ? error.message : "Unknown error"}
        </div>
      </div>
    );
  }

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold">Teams</h1>
          <p className="text-muted-foreground">
            Organize people and agents into teams.
          </p>
        </div>
        <CreateTeamDialog />
      </div>

      {teams.length > 3 && (
        <div className="relative max-w-sm">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
          <Input
            placeholder="Search teams..."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="pl-9"
          />
        </div>
      )}

      {filtered.length === 0 ? (
        <EmptyState
          icon={Users}
          title="No teams"
          description={
            search
              ? "No teams match your search."
              : "Create a team to organize people and share agents."
          }
        />
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {filtered.map((team) => (
            <Link key={team.id} to={`/teams/${team.id}`}>
              <Card className="hover:shadow-md transition-shadow cursor-pointer">
                <CardHeader className="pb-2">
                  <div className="flex items-start justify-between">
                    <CardTitle className="text-base">{team.name}</CardTitle>
                    <div className="rounded-full bg-muted p-2">
                      <Users className="h-4 w-4 text-muted-foreground" />
                    </div>
                  </div>
                </CardHeader>
                <CardContent>
                  <div className="flex items-center gap-2">
                    <Badge variant="secondary">
                      {team.member_count} member{team.member_count !== 1 ? "s" : ""}
                    </Badge>
                    {team.pending_requests > 0 && (
                      <Badge variant="outline">
                        {team.pending_requests} pending
                      </Badge>
                    )}
                  </div>
                </CardContent>
              </Card>
            </Link>
          ))}
        </div>
      )}
    </div>
  );
}

function CreateTeamDialog() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const identity = useAuthStore((s) => s.identity);
  const create = useCreateIdentityTeam();

  const handleCreate = () => {
    if (!name.trim() || !identity?.id) return;
    create.mutate(
      { name, creator_id: identity.id },
      {
        onSuccess: () => {
          setOpen(false);
          setName("");
        },
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm">
          <Plus className="mr-1.5 h-4 w-4" />
          Create Team
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create Team</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <Input
            placeholder="Team name"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <Button
            className="w-full"
            onClick={handleCreate}
            disabled={!name.trim() || !identity?.id || create.isPending}
          >
            {create.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              "Create"
            )}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
