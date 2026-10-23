import { useNavigate } from "react-router";
import { Badge } from "@/components/ui/badge";
import { PrincipalPill } from "@/components/principal-pill";
import { ChannelLabel } from "@/components/sessions/channel-label";
import type { Session } from "@/lib/api/sessions";

interface SessionListTableProps {
  sessions: Session[];
}

export function SessionListTable({ sessions }: SessionListTableProps) {
  const navigate = useNavigate();

  return (
    <div className="border rounded-lg overflow-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b bg-muted/50">
            <th className="text-left font-medium p-3">Label / ID</th>
            <th className="text-left font-medium p-3">Agent</th>
            <th className="text-left font-medium p-3">User</th>
            <th className="text-left font-medium p-3">Channel</th>
            <th className="text-left font-medium p-3">Type</th>
            <th className="text-right font-medium p-3">Messages</th>
            <th className="text-right font-medium p-3">Tokens</th>
            <th className="text-left font-medium p-3">Updated</th>
          </tr>
        </thead>
        <tbody>
          {sessions.map((s) => (
            <tr
              key={s.id}
              onClick={() => navigate(`/sessions/${s.id}`)}
              className="border-b hover:bg-muted/30 cursor-pointer transition-colors"
            >
              <td className="p-3">
                <div className="font-medium truncate max-w-[200px]">{s.label || s.id}</div>
                {s.label && (
                  <div className="text-xs text-muted-foreground truncate max-w-[200px]">{s.id}</div>
                )}
              </td>
              <td className="p-3">
                <span className="truncate max-w-[120px] inline-block">
                  {s.agent_display_name || s.agent_id}
                </span>
              </td>
              <td className="p-3">
                <PrincipalPill
                  principal={{
                    id: s.owner,
                    display_name: s.owner_display_name,
                    kind: s.owner_kind,
                  }}
                />
              </td>
              <td className="p-3">
                <ChannelLabel display={s.channel_display} channel={s.channel} sessionId={s.id} />
              </td>
              <td className="p-3">
                <div className="flex flex-wrap items-center gap-1">
                  <Badge variant={s.session_category === "system" ? "outline" : "secondary"}>
                    {s.session_category}
                  </Badge>
                  {s.is_claude_chat && (
                    <Badge variant="outline" className="text-[10px]">
                      Claude Code
                    </Badge>
                  )}
                </div>
              </td>
              <td className="p-3 text-right tabular-nums">{s.messages}</td>
              <td className="p-3 text-right tabular-nums">{s.total_tokens.toLocaleString()}</td>
              <td className="p-3 text-muted-foreground">{formatRelative(s.updated_at)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function formatRelative(ts: string): string {
  const diff = Date.now() - new Date(ts).getTime();
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(ts).toLocaleDateString();
}
