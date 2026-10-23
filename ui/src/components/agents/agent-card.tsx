import { useNavigate } from "react-router";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Bot, Wrench, Cpu, Users } from "lucide-react";
import type { AgentInfo } from "@/lib/api/agents";

interface AgentCardProps {
  agent: AgentInfo;
  linkTo?: string;
}

export function AgentCard({ agent, linkTo }: AgentCardProps) {
  const navigate = useNavigate();
  const displayName = agent.name ?? agent.id;

  return (
    <Card
      className={`hover:shadow-md transition-shadow ${linkTo ? "cursor-pointer" : ""}`}
      onClick={linkTo ? () => navigate(linkTo) : undefined}
    >
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between">
          <div className="min-w-0 flex-1">
            <CardTitle className="text-base truncate">{displayName}</CardTitle>
            {agent.name && (
              <p className="text-xs text-muted-foreground font-mono mt-0.5 truncate">
                {agent.id}
              </p>
            )}
          </div>
          <div className="rounded-full bg-muted p-2 ml-2 shrink-0">
            <Bot className="h-4 w-4 text-muted-foreground" />
          </div>
        </div>
      </CardHeader>
      <CardContent>
        <div className="flex items-center gap-2 flex-wrap">
          {agent.model && (
            <Badge variant="secondary" className="gap-1">
              <Cpu className="h-3 w-3" />
              {agent.model}
            </Badge>
          )}
          <Badge variant="outline" className="gap-1">
            <Wrench className="h-3 w-3" />
            {agent.capabilities.length} tools
          </Badge>
          {agent.default && <Badge variant="success">default</Badge>}
          {agent.owner_type === "team" && (
            <Badge variant="outline" className="gap-1">
              <Users className="h-3 w-3" />
              Team
            </Badge>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
