import { useMemo, useState, type ComponentType, type ReactNode } from "react";
import { useParams, useNavigate } from "react-router";
import { ArrowLeft, ChevronRight, Download, MessageSquare, Clock, Hash, Cpu, ChevronUp } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { PageSkeleton } from "@/components/loading-skeleton";
import { MessageBubble } from "@/components/chat/message-bubble";
import { ChannelLabel } from "@/components/sessions/channel-label";
import { useSessions, useSessionTranscript, useSessionStats } from "@/hooks/use-sessions";
import { sessionsApi, type SessionStats, type ToolCallSummary } from "@/lib/api/sessions";
import { cn } from "@/lib/utils";

export default function SessionDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();

  const { data: sessionsData, isLoading: sessionsLoading } = useSessions();
  const {
    data: transcriptData,
    isLoading: transcriptLoading,
    fetchNextPage,
    hasNextPage,
    isFetchingNextPage,
  } = useSessionTranscript(id);
  const { data: statsData } = useSessionStats(id);

  const session = sessionsData?.sessions?.find((s) => s.id === id);
  // Pages arrive newest-first; reverse so the conversation reads oldest → newest.
  const entries = useMemo(
    () => [...(transcriptData?.pages ?? [])].reverse().flatMap((p) => p.entries),
    [transcriptData],
  );
  const stats = statsData?.stats;

  const handleExport = async () => {
    if (!id) return;
    try {
      const exportData = await sessionsApi.export(id);
      const blob = new Blob([JSON.stringify(exportData, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `session-${id}.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      alert(`Export failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  if (sessionsLoading || transcriptLoading) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      {/* Header */}
      <div className="flex items-center gap-3 flex-wrap">
        <Button variant="ghost" size="icon" onClick={() => navigate("/sessions")}>
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div className="flex-1 min-w-0">
          <h1 className="text-2xl font-bold truncate">{session?.label ?? id}</h1>
          {session?.label && <p className="text-sm text-muted-foreground truncate">{id}</p>}
        </div>
        <Button variant="outline" onClick={handleExport}>
          <Download className="h-4 w-4 mr-1.5" />
          Export
        </Button>
      </div>

      {/* Metadata cards */}
      {session && (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <MetaCard icon={Cpu} label="Agent" value={session.agent_display_name || session.agent_id} />
          <MetaCard icon={Hash} label="Channel">
            <ChannelLabel
              display={session.channel_display}
              channel={session.channel}
              sessionId={session.id}
            />
          </MetaCard>
          <MetaCard icon={MessageSquare} label="Messages" value={String(session.messages)} />
          <MetaCard icon={Clock} label="Tokens" value={session.total_tokens.toLocaleString()} />
        </div>
      )}

      {/* Session timestamps */}
      {session && (
        <div className="flex gap-4 text-xs text-muted-foreground flex-wrap">
          <span>Created: {new Date(session.created_at).toLocaleString()}</span>
          <span>Updated: {new Date(session.updated_at).toLocaleString()}</span>
          {session.kind && (
            <Badge variant="outline" className="text-[10px]">{session.kind}</Badge>
          )}
        </div>
      )}

      {stats && <StatsPanel stats={stats} />}

      {/* Transcript */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">Transcript</CardTitle>
        </CardHeader>
        <CardContent>
          {entries.length === 0 ? (
            session?.is_claude_chat ? (
              <p className="text-sm text-muted-foreground py-8 text-center max-w-md mx-auto">
                This is a Claude Code session — the conversation runs inside Claude Code and
                isn't mirrored here.
              </p>
            ) : (
              <p className="text-sm text-muted-foreground py-8 text-center">No messages in this session.</p>
            )
          ) : (
            <div className="max-w-3xl mx-auto">
              {hasNextPage && (
                <div className="flex justify-center pb-3">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => fetchNextPage()}
                    disabled={isFetchingNextPage}
                  >
                    <ChevronUp className="h-3.5 w-3.5 mr-1.5" />
                    {isFetchingNextPage ? "Loading…" : "Load earlier messages"}
                  </Button>
                </div>
              )}
              <div className="divide-y divide-border/50">
                {entries.map((entry) => (
                  <MessageBubble
                    key={`${entry.session_id}:${entry.id}`}
                    role={entry.role}
                    content={entry.content}
                    timestamp={entry.timestamp}
                    status="complete"
                    toolMetadata={entry.tool_metadata}
                    agentId={session?.agent_id}
                  />
                ))}
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

function MetaCard({
  icon: Icon,
  label,
  value,
  children,
}: {
  icon: ComponentType<{ className?: string }>;
  label: string;
  value?: string;
  children?: ReactNode;
}) {
  return (
    <Card>
      <CardContent className="p-4 flex items-center gap-3">
        <div className="rounded-md bg-muted p-2">
          <Icon className="h-4 w-4 text-muted-foreground" />
        </div>
        <div className="min-w-0">
          <p className="text-xs text-muted-foreground">{label}</p>
          {children ?? <p className="font-medium text-sm truncate">{value}</p>}
        </div>
      </CardContent>
    </Card>
  );
}

function StatRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between gap-2 py-1 border-b border-dashed last:border-0">
      <span className="text-muted-foreground">{label}</span>
      <span className="font-medium tabular-nums">{value}</span>
    </div>
  );
}

function RoleBreakdown({ counts }: { counts: Record<string, number> }) {
  const entries = Object.entries(counts);
  if (entries.length === 0) return <span className="text-muted-foreground">—</span>;
  return (
    <span className="font-medium tabular-nums">
      {entries
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([role, n]) => `${role}: ${n.toLocaleString()}`)
        .join(" · ")}
    </span>
  );
}

function ToolCallCard({ summary }: { summary: ToolCallSummary }) {
  const [open, setOpen] = useState(false);
  const byTool = Object.entries(summary.by_tool);
  const avgMs = summary.total > 0 ? Math.round(summary.total_duration_ms / summary.total) : 0;
  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="text-sm">Tool calls</CardTitle>
      </CardHeader>
      <CardContent className="text-sm space-y-2">
        <div className="flex flex-wrap gap-x-4 gap-y-1 tabular-nums">
          <span>total <strong>{summary.total.toLocaleString()}</strong></span>
          <span>ok <strong className="text-emerald-500">{summary.succeeded.toLocaleString()}</strong></span>
          <span>fail <strong className={cn(summary.failed > 0 && "text-red-500")}>{summary.failed.toLocaleString()}</strong></span>
          <span>avg <strong>{avgMs.toLocaleString()}ms</strong></span>
        </div>
        {byTool.length === 0 ? (
          <p className="text-xs text-muted-foreground">No tool calls in this session.</p>
        ) : (
          <Collapsible open={open} onOpenChange={setOpen}>
            <CollapsibleTrigger className="text-xs">
              <ChevronRight className={cn("h-3.5 w-3.5 transition-transform", open && "rotate-90")} />
              {open ? "Hide" : "Show"} per-tool breakdown ({byTool.length})
            </CollapsibleTrigger>
            <CollapsibleContent className="mt-2">
              <table className="w-full text-xs">
                <thead className="text-muted-foreground">
                  <tr className="text-left">
                    <th className="py-1 pr-4 font-normal">Tool</th>
                    <th className="py-1 pr-4 font-normal text-right">Calls</th>
                    <th className="py-1 pr-4 font-normal text-right">OK</th>
                    <th className="py-1 pr-4 font-normal text-right">Fail</th>
                    <th className="py-1 font-normal text-right">Total ms</th>
                  </tr>
                </thead>
                <tbody className="tabular-nums">
                  {byTool
                    .sort(([, a], [, b]) => b.count - a.count)
                    .map(([name, s]) => (
                      <tr key={name} className="border-t border-border/50">
                        <td className="py-1 pr-4 font-mono">{name}</td>
                        <td className="py-1 pr-4 text-right">{s.count.toLocaleString()}</td>
                        <td className="py-1 pr-4 text-right text-emerald-500">{s.succeeded.toLocaleString()}</td>
                        <td className={cn("py-1 pr-4 text-right", s.failed > 0 && "text-red-500")}>{s.failed.toLocaleString()}</td>
                        <td className="py-1 text-right">{s.total_duration_ms.toLocaleString()}</td>
                      </tr>
                    ))}
                </tbody>
              </table>
            </CollapsibleContent>
          </Collapsible>
        )}
      </CardContent>
    </Card>
  );
}

function StatsPanel({ stats }: { stats: SessionStats }) {
  const rows: Array<[string, string]> = [
    ["message_count", stats.message_count.toLocaleString()],
    ["total_tokens", stats.total_tokens.toLocaleString()],
    ["total_input_tokens", stats.total_input_tokens.toLocaleString()],
    ["total_output_tokens", stats.total_output_tokens.toLocaleString()],
  ];
  if (stats.duration_seconds != null) {
    rows.push(["duration_seconds", stats.duration_seconds.toLocaleString()]);
  }
  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">Stats</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3 text-sm">
            {rows.map(([label, value]) => (
              <StatRow key={label} label={label} value={value} />
            ))}
            <div className="flex justify-between gap-2 py-1 border-b border-dashed last:border-0 sm:col-span-2 lg:col-span-3">
              <span className="text-muted-foreground">messages_by_role</span>
              <RoleBreakdown counts={stats.messages_by_role} />
            </div>
            <div className="flex justify-between gap-2 py-1 border-b border-dashed last:border-0 sm:col-span-2 lg:col-span-3">
              <span className="text-muted-foreground">tokens_by_role</span>
              <RoleBreakdown counts={stats.tokens_by_role} />
            </div>
          </div>
        </CardContent>
      </Card>
      <ToolCallCard summary={stats.tool_calls} />
    </div>
  );
}
