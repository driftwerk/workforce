import { Link } from "react-router";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/badge";
import { useTaskTrace } from "@/hooks/use-stats";
import { formatCost, formatDuration } from "@/lib/utils";

interface Props {
  taskId: string | null;
  onClose: () => void;
}

export function TraceDrawer({ taskId, onClose }: Props) {
  const { data, isLoading, error } = useTaskTrace(taskId ?? undefined);

  return (
    <Dialog open={!!taskId} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="!left-auto !right-0 !top-0 !translate-x-0 !translate-y-0 !max-w-2xl h-screen overflow-y-auto rounded-none data-[state=open]:slide-in-from-right-0 data-[state=closed]:slide-out-to-right-0">
        <DialogTitle className="text-base">Task trace</DialogTitle>
        {isLoading && <div className="text-sm text-muted-foreground">Loading…</div>}
        {error && <div className="text-sm text-red-500">Failed to load trace.</div>}
        {data && (
          <div className="space-y-4 text-sm">
            <section className="rounded-md border border-border p-3 space-y-1.5">
              <div className="flex items-center justify-between">
                <h3 className="font-medium">{data.task.task_type ?? "Task"}</h3>
                <Badge variant={data.task.success ? "default" : "destructive"}>
                  {data.task.success ? "success" : data.task.error_type ?? "failed"}
                </Badge>
              </div>
              <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs text-muted-foreground">
                <dt>Agent</dt>
                <dd className="text-foreground">{data.task.agent_id}</dd>
                <dt>Identity</dt>
                <dd className="text-foreground">
                  {data.identity ? (
                    <Link
                      to={`/users/${data.identity.id}?tab=usage`}
                      className="underline hover:text-primary"
                    >
                      {data.identity.display_name}
                    </Link>
                  ) : (
                    <span className="italic">system</span>
                  )}
                </dd>
                <dt>Model</dt>
                <dd className="text-foreground">{data.task.model}</dd>
                <dt>Cost</dt>
                <dd className="text-foreground">{formatCost(data.task.cost_usd)}</dd>
                <dt>Duration</dt>
                <dd className="text-foreground">{formatDuration(data.task.duration_ms)}</dd>
                <dt>Rounds</dt>
                <dd className="text-foreground">{data.task.rounds}</dd>
                <dt>Tools</dt>
                <dd className="text-foreground">{data.task.tool_invocation_count}</dd>
                {data.task.task_id && (
                  <>
                    <dt>Task ID</dt>
                    <dd className="font-mono text-foreground">{data.task.task_id}</dd>
                  </>
                )}
                {data.task.ticket_id && (
                  <>
                    <dt>Ticket</dt>
                    <dd className="text-foreground">{data.task.ticket_id}</dd>
                  </>
                )}
              </dl>
            </section>

            {data.rounds.length > 0 && (
              <section>
                <h4 className="text-xs font-semibold text-muted-foreground mb-2">Rounds</h4>
                <ol className="space-y-2">
                  {data.rounds.map((r) => (
                    <li key={r.round_number} className="rounded-md border border-border p-2 text-xs">
                      <div className="flex items-center justify-between">
                        <span className="font-medium">Round {r.round_number}</span>
                        <span className="text-muted-foreground">
                          {formatDuration(r.latency_ms)} · {r.input_tokens.toLocaleString()} in / {r.output_tokens.toLocaleString()} out
                        </span>
                      </div>
                      <div className="text-muted-foreground mt-1">stop: {r.stop_reason}{r.tool_count > 0 && ` · ${r.tool_count} tool calls`}</div>
                    </li>
                  ))}
                </ol>
              </section>
            )}

            {data.tools.length > 0 && (
              <section>
                <h4 className="text-xs font-semibold text-muted-foreground mb-2">Tool calls</h4>
                <div className="rounded-md border border-border overflow-hidden">
                  <table className="w-full text-xs">
                    <thead className="bg-muted/40">
                      <tr>
                        <th className="px-2 py-1 text-left">Round</th>
                        <th className="px-2 py-1 text-left">Tool</th>
                        <th className="px-2 py-1 text-left">Outcome</th>
                        <th className="px-2 py-1 text-right">Duration</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y divide-border">
                      {data.tools.map((t, i) => (
                        <tr key={`${t.round_number}-${i}`}>
                          <td className="px-2 py-1">{t.round_number}</td>
                          <td className="px-2 py-1 font-mono">{t.tool_name}</td>
                          <td className="px-2 py-1">
                            {t.success ? (
                              <span className="text-green-600">ok</span>
                            ) : (
                              <span className="text-red-500">{t.denial_reason ?? "failed"}</span>
                            )}
                          </td>
                          <td className="px-2 py-1 text-right tabular-nums">
                            {formatDuration(t.duration_ms)}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </section>
            )}

            {data.cost_records.length > 0 && (
              <section>
                <h4 className="text-xs font-semibold text-muted-foreground mb-2">
                  Cost records ({data.cost_records.length})
                </h4>
                <div className="rounded-md border border-border overflow-hidden">
                  <table className="w-full text-xs">
                    <thead className="bg-muted/40">
                      <tr>
                        <th className="px-2 py-1 text-left">When</th>
                        <th className="px-2 py-1 text-left">Model</th>
                        <th className="px-2 py-1 text-right">Tokens</th>
                        <th className="px-2 py-1 text-right">Cost</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y divide-border">
                      {data.cost_records.map((c) => (
                        <tr key={c.timestamp + c.model}>
                          <td className="px-2 py-1">
                            {new Date(c.timestamp).toLocaleString()}
                          </td>
                          <td className="px-2 py-1 font-mono">{c.model}</td>
                          <td className="px-2 py-1 text-right tabular-nums">
                            {(c.input_tokens + c.output_tokens).toLocaleString()}
                          </td>
                          <td className="px-2 py-1 text-right tabular-nums">
                            {formatCost(c.cost_usd)}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </section>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
