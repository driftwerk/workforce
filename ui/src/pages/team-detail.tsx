import { useState, useMemo } from "react";
import { useParams, useNavigate } from "react-router";
import { ArrowLeft, Users, Crown, Bot, Check, X, UserPlus, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useIdentityTeam, useTeamMutation } from "@/hooks/use-teams";
import { useIdentities } from "@/hooks/use-identities";
import { useAgents } from "@/hooks/use-agents";
import { CreateAgentDialog } from "@/components/agents/create-agent-dialog";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { gateway } from "@/lib/api-client";

export default function TeamDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const { data, isLoading, error } = useIdentityTeam(id);

  if (isLoading) return <PageSkeleton />;

  if (error || !data) {
    return (
      <div className="p-6">
        <div className="text-destructive">Team not found.</div>
      </div>
    );
  }

  return <IdentityTeamDetail team={data} onBack={() => navigate("/teams")} />;
}

function IdentityTeamDetail({
  team,
  onBack,
}: {
  team: import("@/lib/api/teams").IdentityTeamDetail;
  onBack: () => void;
}) {
  const mutations = useTeamMutation();
  const { data: identitiesData } = useIdentities();
  const [addSearch, setAddSearch] = useState("");
  const [showAddMember, setShowAddMember] = useState(false);

  const identityMap = useMemo(() => {
    const map: Record<string, string> = {};
    for (const id of identitiesData?.identities ?? []) {
      map[id.id] = id.display_name;
    }
    return map;
  }, [identitiesData]);

  const resolveName = (uuid: string) => identityMap[uuid] ?? uuid.slice(0, 8) + "...";

  const availableUsers = useMemo(() => {
    const memberSet = new Set(team.members);
    return (identitiesData?.identities ?? [])
      .filter((u) => !memberSet.has(u.id))
      .filter((u) =>
        !addSearch || u.display_name.toLowerCase().includes(addSearch.toLowerCase()) ||
        u.email?.toLowerCase().includes(addSearch.toLowerCase())
      );
  }, [identitiesData, team.members, addSearch]);

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="icon" onClick={onBack} aria-label="Back to teams">
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div>
          <h1 className="text-2xl font-bold">{team.name}</h1>
          <p className="text-muted-foreground font-mono text-sm">{team.id}</p>
        </div>
      </div>

      <div className="grid gap-4 sm:grid-cols-3">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground flex items-center gap-1.5">
              <Users className="h-3.5 w-3.5" />Members
            </CardTitle>
          </CardHeader>
          <CardContent>
            <span className="text-2xl font-bold">{team.members.length}</span>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground flex items-center gap-1.5">
              <UserPlus className="h-3.5 w-3.5" />Pending Requests
            </CardTitle>
          </CardHeader>
          <CardContent>
            <span className="text-2xl font-bold">{team.join_requests.length}</span>
          </CardContent>
        </Card>
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-sm font-medium text-muted-foreground flex items-center gap-1.5">
              <Crown className="h-3.5 w-3.5" />Creator
            </CardTitle>
          </CardHeader>
          <CardContent>
            <span className="text-sm font-medium">{resolveName(team.creator_id)}</span>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="text-base">Members</CardTitle>
          <Button size="sm" variant="outline" onClick={() => setShowAddMember(!showAddMember)}>
            <Plus className="mr-1 h-3.5 w-3.5" />Add Member
          </Button>
        </CardHeader>
        <CardContent className="space-y-3">
          {showAddMember && (
            <div className="rounded-md border p-3 space-y-2 bg-muted/30">
              <Input
                placeholder="Search users by name or email..."
                value={addSearch}
                onChange={(e) => setAddSearch(e.target.value)}
                aria-label="Search users to add"
              />
              <div className="max-h-40 overflow-y-auto space-y-1">
                {availableUsers.length === 0 ? (
                  <p className="text-xs text-muted-foreground p-2">No users found</p>
                ) : (
                  availableUsers.slice(0, 10).map((u) => (
                    <button
                      key={u.id}
                      className="flex w-full items-center justify-between rounded px-2 py-1.5 text-sm hover:bg-accent"
                      onClick={() => {
                        mutations.addMember.mutate({ teamId: team.id, identityId: u.id });
                        setShowAddMember(false);
                        setAddSearch("");
                      }}
                    >
                      <span>{u.display_name}</span>
                      <span className="text-xs text-muted-foreground">{u.email ?? ""}</span>
                    </button>
                  ))
                )}
              </div>
            </div>
          )}
          {team.members.length > 0 ? (
            <div className="space-y-2">
              {team.members.map((memberId) => (
                <div key={memberId} className="flex items-center justify-between rounded-md border px-3 py-2">
                  <span className="text-sm font-medium">{resolveName(memberId)}</span>
                  <div className="flex items-center gap-1">
                    {memberId === team.creator_id && <Badge variant="secondary">Creator</Badge>}
                    {memberId !== team.creator_id && (
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 text-xs text-destructive"
                        onClick={() => mutations.removeMember.mutate({ teamId: team.id, memberId })}
                      >
                        Remove
                      </Button>
                    )}
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <p className="text-muted-foreground text-sm">No members.</p>
          )}
        </CardContent>
      </Card>

      {team.join_requests.length > 0 && (
        <Card>
          <CardHeader><CardTitle className="text-base">Join Requests</CardTitle></CardHeader>
          <CardContent>
            <div className="space-y-2">
              {team.join_requests.map((req) => (
                <div key={req.identity_id} className="flex items-center justify-between rounded-md border px-3 py-2">
                  <div>
                    <span className="text-sm font-medium">{resolveName(req.identity_id)}</span>
                    <p className="text-xs text-muted-foreground">{new Date(req.requested_at).toLocaleDateString()}</p>
                  </div>
                  <div className="flex gap-1">
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={() => mutations.approveJoin.mutate({ teamId: team.id, applicantId: req.identity_id })}
                    >
                      <Check className="mr-1 h-3 w-3" />Approve
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-7 text-xs text-destructive"
                      onClick={() => mutations.rejectJoin.mutate({ teamId: team.id, applicantId: req.identity_id })}
                    >
                      <X className="mr-1 h-3 w-3" />Reject
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      <TeamAgentsSection teamId={team.id} />
    </div>
  );
}

function TeamAgentsSection({ teamId }: { teamId: string }) {
  const { data: agentsData } = useAgents();
  const [showAssign, setShowAssign] = useState(false);
  const [search, setSearch] = useState("");
  const [error, setError] = useState("");
  const qc = useQueryClient();

  const assignMutation = useMutation({
    mutationFn: (agentId: string) =>
      gateway.put(`/api/agents/${agentId}`, {
        owner_id: teamId,
        owner_type: "team",
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agents"] });
      setShowAssign(false);
      setSearch("");
      setError("");
    },
    onError: (e: Error) => setError(e.message),
  });

  const unassignMutation = useMutation({
    mutationFn: (agentId: string) =>
      gateway.put(`/api/agents/${agentId}`, {
        owner_id: "",
        owner_type: "user",
      }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agents"] });
      setError("");
    },
    onError: (e: Error) => setError(e.message),
  });

  const allAgents = agentsData?.agents ?? [];
  const teamAgents = allAgents.filter(
    (a) => a.owner_id === teamId && a.owner_type === "team",
  );
  const availableAgents = allAgents
    .filter((a) => a.owner_id !== teamId)
    .filter(
      (a) =>
        !search ||
        a.id.toLowerCase().includes(search.toLowerCase()) ||
        a.name?.toLowerCase().includes(search.toLowerCase()),
    );

  const [createOpen, setCreateOpen] = useState(false);

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between">
        <CardTitle className="text-base flex items-center gap-1.5">
          <Bot className="h-4 w-4" />
          Agents
        </CardTitle>
        <div className="flex gap-2">
          <Button size="sm" variant="outline" onClick={() => setCreateOpen(true)}>
            <Plus className="mr-1 h-3.5 w-3.5" />
            Create Agent
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={() => setShowAssign(!showAssign)}
        >
          <Plus className="mr-1 h-3.5 w-3.5" />
          Assign Agent
        </Button>
        </div>
      </CardHeader>
      <CreateAgentDialog open={createOpen} onOpenChange={setCreateOpen} ownerId={teamId} ownerType="team" />
      <CardContent className="space-y-3">
        {error && (
          <p className="text-sm text-destructive" role="alert">{error}</p>
        )}
        {showAssign && (
          <div className="rounded-md border p-3 space-y-2 bg-muted/30">
            <Input
              placeholder="Search agents by name or ID..."
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              aria-label="Search agents to assign"
            />
            <div className="max-h-40 overflow-y-auto space-y-1">
              {availableAgents.length === 0 ? (
                <p className="text-xs text-muted-foreground p-2">
                  No available agents
                </p>
              ) : (
                availableAgents.slice(0, 10).map((a) => (
                  <button
                    type="button"
                    key={a.id}
                    className="flex w-full items-center justify-between rounded px-2 py-1.5 text-sm hover:bg-accent"
                    onClick={() => assignMutation.mutate(a.id)}
                  >
                    <span>{a.name ?? a.id}</span>
                    <span className="text-xs text-muted-foreground">
                      {a.model ?? ""}
                    </span>
                  </button>
                ))
              )}
            </div>
          </div>
        )}
        {teamAgents.length > 0 ? (
          <div className="space-y-2">
            {teamAgents.map((a) => (
              <div
                key={a.id}
                className="flex items-center justify-between rounded-md border px-3 py-2"
              >
                <div className="flex items-center gap-2">
                  <Bot className="h-4 w-4 text-muted-foreground" />
                  <span className="text-sm font-medium">
                    {a.name ?? a.id}
                  </span>
                  <span className="text-xs text-muted-foreground">
                    {a.model ?? ""}
                  </span>
                </div>
                <Button
                  size="sm"
                  variant="ghost"
                  className="text-xs text-destructive"
                  onClick={() => {
                    if (confirm(`Remove "${a.name ?? a.id}" from this team?`)) {
                      unassignMutation.mutate(a.id);
                    }
                  }}
                >
                  <X className="h-3 w-3" />
                </Button>
              </div>
            ))}
          </div>
        ) : (
          <p className="text-muted-foreground text-sm">
            No agents assigned to this team.
          </p>
        )}
      </CardContent>
    </Card>
  );
}
