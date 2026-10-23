import { useState, useMemo } from "react";
import {
  Clock,
  Heart,
  Bell,
  Plus,
  Play,
  Trash2,
  Pencil,
  Loader2,
  AlertCircle,
  History,
  Globe,
  Lock,
  Pin,
  RefreshCw,
  Send,
} from "lucide-react";
import { useScheduledJobHistory } from "@/hooks/use-cost";
import { useAuthStore } from "@/stores/auth-store";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PrincipalPill } from "@/components/principal-pill";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogTrigger,
} from "@/components/ui/dialog";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import {
  useScheduledJobs,
  useCreateJob,
  useUpdateJob,
  useDeleteJob,
  useTriggerJob,
  useJobHistory,
} from "@/hooks/use-scheduler";
import {
  useHeartbeatChecks,
  useHeartbeatCheckDetail,
  useHeartbeatStatus,
  useCreateHeartbeatCheck,
  useUpdateHeartbeatCheck,
  useDeleteHeartbeatCheck,
  useTriggerHeartbeatCheck,
  useHeartbeatHistory,
} from "@/hooks/use-heartbeat";
import { useHasCapability } from "@/hooks/use-permissions";
import { updateRefusal } from "@/lib/api/scheduler";
import type { CostConfirmation, ScheduledJob, UpdateJobRequest } from "@/lib/api/scheduler";
import { formatCostScaled, formatDuration } from "@/lib/utils";

// ── Helpers ──────────────────────────────────────────────────────────────────

function scheduleLabel(schedule: ScheduledJob["schedule"]): string {
  if (!schedule) return "—";
  if ("expr" in schedule) return schedule.expr;
  if ("every_secs" in schedule) return `every ${schedule.every_secs}s`;
  if ("at" in schedule) return `at ${schedule.at}`;
  return "—";
}

// System admins manage every job; everyone else only their own. A token-only
// login (no identity) is the static gateway admin, mirroring the sidebar.
function useScheduleAccess() {
  const { identity } = useAuthStore();
  const isAdmin = identity ? identity.role === "system_admin" : true;
  const canMutate = (job: ScheduledJob) =>
    isAdmin || (!!job.owner && job.owner === identity?.id);
  return { isAdmin, canMutate };
}

function OwnerCell({ job }: { job: ScheduledJob }) {
  return (
    <PrincipalPill
      principal={
        job.owner
          ? { id: job.owner, display_name: job.owner_display_name, kind: job.owner_kind }
          : null
      }
      placeholder="System"
    />
  );
}

function VisibilityBadge({ visibility }: { visibility: ScheduledJob["visibility"] }) {
  const isPublic = visibility === "global";
  return (
    <Badge variant="outline" className="gap-1 text-xs">
      {isPublic ? <Globe className="h-3 w-3" /> : <Lock className="h-3 w-3" />}
      {isPublic ? "Public" : "Private"}
    </Badge>
  );
}

function VisibilityToggle({ isPublic, onChange }: { isPublic: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className="flex items-center justify-between rounded-md border px-3 py-2">
      <div>
        <p className="text-sm font-medium">Visibility</p>
        <p className="text-xs text-muted-foreground">
          {isPublic
            ? "Public — visible to everyone in the workspace."
            : "Private — only you and admins can see this."}
        </p>
      </div>
      <Button
        type="button"
        size="sm"
        variant={isPublic ? "default" : "outline"}
        className="gap-1.5"
        onClick={() => onChange(!isPublic)}
      >
        {isPublic ? <Globe className="h-3.5 w-3.5" /> : <Lock className="h-3.5 w-3.5" />}
        {isPublic ? "Public" : "Private"}
      </Button>
    </div>
  );
}

function SessionBadge({ session }: { session: ScheduledJob["session"] }) {
  if (!session) return <span className="text-xs text-muted-foreground">—</span>;
  const pinned = session === "pinned";
  return (
    <Badge variant={pinned ? "secondary" : "outline"} className="gap-1 text-xs">
      {pinned ? <Pin className="h-3 w-3" /> : <RefreshCw className="h-3 w-3" />}
      {pinned ? "Pinned" : "Fresh"}
    </Badge>
  );
}

function SessionToggle({ pinned, onChange }: { pinned: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className="flex items-center justify-between rounded-md border px-3 py-2">
      <div>
        <p className="text-sm font-medium">Session</p>
        <p className="text-xs text-muted-foreground">
          {pinned
            ? "Pinned — every run replays all the job's earlier runs, so each run costs more than the one before."
            : "Fresh — every run starts in a new session. (Default.)"}
        </p>
      </div>
      <Button
        type="button"
        size="sm"
        variant={pinned ? "default" : "outline"}
        className="gap-1.5"
        onClick={() => onChange(!pinned)}
      >
        {pinned ? <Pin className="h-3.5 w-3.5" /> : <RefreshCw className="h-3.5 w-3.5" />}
        {pinned ? "Pinned" : "Fresh"}
      </Button>
    </div>
  );
}

function DeliveryToggle({
  agentOwns,
  onChange,
}: {
  agentOwns: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between rounded-md border px-3 py-2">
      <div>
        <p className="text-sm font-medium">Delivery</p>
        <p className="text-xs text-muted-foreground">
          {agentOwns
            ? "Agent posts its own result; the scheduler won't also send the reply. Use for jobs whose prompt posts to a channel (avoids a duplicate message)."
            : "The scheduler delivers the agent's reply to the destination. (Default.)"}
        </p>
      </div>
      <Button
        type="button"
        size="sm"
        variant={agentOwns ? "default" : "outline"}
        className="gap-1.5"
        onClick={() => onChange(!agentOwns)}
      >
        {agentOwns ? <Send className="h-3.5 w-3.5" /> : <Bell className="h-3.5 w-3.5" />}
        {agentOwns ? "Agent posts" : "Scheduler"}
      </Button>
    </div>
  );
}

// ── Create Schedule Dialog ───────────────────────────────────────────────────

function CreateScheduleDialog() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [message, setMessage] = useState("");
  const [cronExpr, setCronExpr] = useState("");
  const [isPublic, setIsPublic] = useState(false);
  const [agentOwnsDelivery, setAgentOwnsDelivery] = useState(false);
  const [error, setError] = useState("");
  const create = useCreateJob();

  const handleCreate = () => {
    setError("");
    if (!name.trim() || !message.trim() || !cronExpr.trim()) return;
    create.mutate(
      {
        name,
        message,
        schedule: { kind: "cron", expr: cronExpr },
        visibility: isPublic ? "global" : "private",
        agent_owns_delivery: agentOwnsDelivery,
      },
      {
        onSuccess: () => {
          setOpen(false);
          setName("");
          setMessage("");
          setCronExpr("");
          setIsPublic(false);
          setAgentOwnsDelivery(false);
        },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm">
          <Plus className="mr-1.5 h-4 w-4" />
          Create Schedule
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create Scheduled Job</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <div>
            <label htmlFor="sched-name" className="text-sm font-medium">Name</label>
            <Input id="sched-name" placeholder="e.g. daily-report" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div>
            <label htmlFor="sched-cron" className="text-sm font-medium">Cron expression</label>
            <Input id="sched-cron" placeholder="e.g. 0 9 * * * (daily at 9am)" value={cronExpr} onChange={(e) => setCronExpr(e.target.value)} />
          </div>
          <div>
            <label htmlFor="sched-msg" className="text-sm font-medium">Message / task</label>
            <Input id="sched-msg" placeholder="What should the agent do?" value={message} onChange={(e) => setMessage(e.target.value)} />
          </div>
          <VisibilityToggle isPublic={isPublic} onChange={setIsPublic} />
          <DeliveryToggle agentOwns={agentOwnsDelivery} onChange={setAgentOwnsDelivery} />
          {error && <p className="text-sm text-destructive">{error}</p>}
          <Button className="w-full" onClick={handleCreate} disabled={!name.trim() || !message.trim() || !cronExpr.trim() || create.isPending}>
            {create.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ── Edit Schedule Dialog ─────────────────────────────────────────────────────

function EditScheduleDialog({ job, onDone }: { job: ScheduledJob; onDone: () => void }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(job.name);
  const [cronExpr, setCronExpr] = useState("expr" in job.schedule ? job.schedule.expr : "");
  const [enabled, setEnabled] = useState(job.enabled);
  const [isPublic, setIsPublic] = useState(job.visibility === "global");
  const [pinned, setPinned] = useState(job.session === "pinned");
  const [error, setError] = useState("");
  const [cost, setCost] = useState<CostConfirmation | null>(null);
  const update = useUpdateJob();

  const handleSave = () => {
    setError("");
    const data: UpdateJobRequest = cost ? { confirmed: true } : {};
    if (name !== job.name) data.name = name;
    if (enabled !== job.enabled) data.enabled = enabled;
    if ("expr" in job.schedule && cronExpr !== job.schedule.expr) {
      data.schedule = { kind: "cron", expr: cronExpr };
    }
    if (!job.is_reminder && isPublic !== (job.visibility === "global")) {
      data.visibility = isPublic ? "global" : "private";
    }
    if (job.session && pinned !== (job.session === "pinned")) {
      data.session = pinned ? "pinned" : "fresh";
    }
    update.mutate(
      { id: job.id, data },
      {
        onSuccess: () => { setCost(null); setOpen(false); onDone(); },
        onError: (e) => {
          const refusal = updateRefusal(e);
          if ("confirmation" in refusal) setCost(refusal.confirmation);
          else setError(refusal.message);
        },
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={(next) => { setCost(null); setOpen(next); }}>
      <DialogTrigger asChild>
        <Button size="sm" variant="ghost" className="h-7 text-xs">
          <Pencil className="h-3 w-3" />
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Edit Schedule</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <div>
            <label className="text-sm font-medium">Name</label>
            <Input value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div>
            <label className="text-sm font-medium">Cron expression</label>
            <Input value={cronExpr} onChange={(e) => { setCost(null); setCronExpr(e.target.value); }} />
          </div>
          {job.message && (
            <div>
              <label className="text-sm font-medium">Agent prompt</label>
              <div className="rounded-md border bg-muted/50 px-3 py-2 text-sm whitespace-pre-wrap max-h-32 overflow-y-auto">
                {job.message}
              </div>
            </div>
          )}
          <div className="flex items-center gap-2">
            <label className="text-sm font-medium">Enabled</label>
            <Button
              size="sm"
              variant={enabled ? "default" : "outline"}
              onClick={() => setEnabled(!enabled)}
            >
              {enabled ? "Active" : "Paused"}
            </Button>
          </div>
          {!job.is_reminder && (
            <VisibilityToggle isPublic={isPublic} onChange={setIsPublic} />
          )}
          {job.session && (
            <SessionToggle pinned={pinned} onChange={(next) => { setCost(null); setPinned(next); }} />
          )}
          {cost && (
            <div className="rounded-md border border-amber-500/50 bg-amber-500/10 px-3 py-2 text-sm">
              <p className="font-medium">This change costs more — confirm to save it.</p>
              <p className="text-xs text-muted-foreground">
                About ${cost.estimate.per_day_usd.toFixed(2)} a day (${cost.estimate.per_run_usd.toFixed(2)} a run,{" "}
                {cost.estimate.runs_per_day} runs a day): {cost.reason}.
              </p>
            </div>
          )}
          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
          <Button onClick={handleSave} disabled={update.isPending}>
            {update.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : cost ? "Confirm and save" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

// ── Job History Dialog ───────────────────────────────────────────────────────

function JobHistoryDialog({ jobId, jobName }: { jobId: string; jobName: string }) {
  const [open, setOpen] = useState(false);
  const { data, isLoading } = useJobHistory(open ? jobId : undefined);
  const history = data?.history ?? [];

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm" variant="ghost" className="h-7 text-xs">
          <History className="h-3 w-3" />
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>Run History — {jobName}</DialogTitle>
        </DialogHeader>
        {isLoading ? (
          <div className="flex justify-center py-8"><Loader2 className="h-5 w-5 animate-spin" /></div>
        ) : history.length === 0 ? (
          <p className="text-sm text-muted-foreground py-8 text-center">No runs yet.</p>
        ) : (
          <div className="max-h-80 overflow-y-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Time</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Duration</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Status</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Output</th>
                </tr>
              </thead>
              <tbody>
                {history.map((r, i) => (
                  <tr key={i} className="border-b">
                    <td className="px-3 py-2 text-xs text-muted-foreground whitespace-nowrap">
                      {new Date(r.started_at).toLocaleString()}
                    </td>
                    <td className="px-3 py-2 text-xs tabular-nums">{formatDuration(r.duration_ms)}</td>
                    <td className="px-3 py-2">
                      <Badge variant={r.success ? "secondary" : "destructive"} className="text-[10px]">
                        {r.success ? "OK" : "Failed"}
                      </Badge>
                    </td>
                    <td className="px-3 py-2 text-xs text-muted-foreground max-w-xs">
                      <p className="truncate">{r.output || "—"}</p>
                      {r.note && <p className="italic">{r.note}</p>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

// ── Job Row ──────────────────────────────────────────────────────────────────

function JobRow({
  job,
  onTriggered,
  showAgent,
  showVisibility,
  showSession,
  canMutate,
}: {
  job: ScheduledJob;
  onTriggered: () => void;
  showAgent?: boolean;
  showVisibility?: boolean;
  showSession?: boolean;
  canMutate: boolean;
}) {
  const trigger = useTriggerJob();
  const del = useDeleteJob();
  const [confirmDelete, setConfirmDelete] = useState(false);

  return (
    <tr className="border-b hover:bg-muted/50">
      <td className="px-4 py-3 text-sm font-medium">{job.name}</td>
      {showAgent && <td className="px-4 py-3 text-xs text-muted-foreground font-mono">{job.agent_id ?? "\u2014"}</td>}
      <td className="px-4 py-3"><OwnerCell job={job} /></td>
      <td className="px-4 py-3 font-mono text-xs text-muted-foreground">{scheduleLabel(job.schedule)}</td>
      {showVisibility && <td className="px-4 py-3"><VisibilityBadge visibility={job.visibility} /></td>}
      <td className="px-4 py-3">
        <Badge variant={job.enabled ? "secondary" : "outline"}>{job.enabled ? "Active" : "Paused"}</Badge>
      </td>
      <td className="px-4 py-3 text-xs text-muted-foreground">{job.next_run ? new Date(job.next_run).toLocaleString() : "—"}</td>
      {showSession && <td className="px-4 py-3"><SessionBadge session={job.session} /></td>}
      <td className="px-4 py-3 text-xs text-muted-foreground">{job.run_count}</td>
      {showSession && (
        <td className="px-4 py-3 text-xs tabular-nums text-muted-foreground">
          {job.spent_usd == null ? "—" : `$${job.spent_usd.toFixed(2)}`}
        </td>
      )}
      <td className="px-4 py-3">
        {canMutate ? (
          <div className="flex gap-1">
            <EditScheduleDialog job={job} onDone={onTriggered} />
            <JobHistoryDialog jobId={job.id} jobName={job.name} />
            <Button size="sm" variant="ghost" className="h-7 text-xs" onClick={() => trigger.mutate(job.id, { onSuccess: onTriggered })} disabled={trigger.isPending}>
              <Play className="h-3 w-3" />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-7 text-xs text-destructive"
              onClick={() => {
                if (!confirmDelete) { setConfirmDelete(true); return; }
                del.mutate(job.id, { onSuccess: onTriggered });
                setConfirmDelete(false);
              }}
            >
              {confirmDelete ? "Confirm?" : <Trash2 className="h-3 w-3" />}
            </Button>
          </div>
        ) : (
          <span className="text-xs text-muted-foreground">—</span>
        )}
      </td>
    </tr>
  );
}

// ── Schedules Tab ────────────────────────────────────────────────────────────

export function SchedulesTab({ agentId }: { agentId?: string } = {}) {
  const { data, isLoading, error, refetch } = useScheduledJobs();
  const { canMutate } = useScheduleAccess();
  const jobs = (data?.jobs ?? []).filter((j: ScheduledJob) => !j.is_reminder);
  const hasAgentColumn = !agentId && jobs.some((j: ScheduledJob) => j.agent_id);

  if (isLoading) return <PageSkeleton />;
  if (error) return <ErrorCard message={error.message} />;

  return (
    <div className="space-y-4">
      <div className="flex justify-between items-center">
        <p className="text-sm text-muted-foreground">Periodic jobs that run on a cron schedule or interval.</p>
        <CreateScheduleDialog />
      </div>
      {jobs.length === 0 ? (
        <EmptyState icon={Clock} title="No scheduled jobs" description="Create a schedule to run agent tasks periodically." />
      ) : (
        <Card>
          <CardContent className="p-0">
            <table className="w-full">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Name</th>
                  {hasAgentColumn && <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Agent</th>}
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Owner</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Schedule</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Visibility</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Status</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Next Run</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Session</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Runs</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Spent</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground"></th>
                </tr>
              </thead>
              <tbody>
                {jobs.map((j: ScheduledJob) => (
                  <JobRow key={j.id} job={j} onTriggered={() => refetch()} showAgent={hasAgentColumn} showVisibility showSession canMutate={canMutate(j)} />
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

// ── Heartbeats Tab ───────────────────────────────────────────────────────────

function CreateHeartbeatDialog({ onCreated }: { onCreated: () => void }) {
  const [open, setOpen] = useState(false);
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [body, setBody] = useState("");
  const [schedule, setSchedule] = useState("");
  const [priority, setPriority] = useState("5");
  const [tags, setTags] = useState("");
  const [error, setError] = useState("");
  const create = useCreateHeartbeatCheck();

  const handleCreate = () => {
    setError("");
    if (!id.trim() || !name.trim() || !body.trim()) return;
    create.mutate(
      {
        id: id.trim(),
        name: name.trim(),
        body: body.trim(),
        schedule: schedule.trim() || undefined,
        priority: parseInt(priority, 10) || 5,
        tags: tags.trim() ? tags.split(",").map((t) => t.trim()).filter(Boolean) : undefined,
      },
      {
        onSuccess: () => {
          setOpen(false);
          setId(""); setName(""); setBody(""); setSchedule(""); setPriority("5"); setTags("");
          onCreated();
        },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm"><Plus className="mr-1.5 h-4 w-4" />New Check</Button>
      </DialogTrigger>
      <DialogContent className="max-w-lg">
        <DialogHeader><DialogTitle>Create Heartbeat Check</DialogTitle></DialogHeader>
        <div className="space-y-3">
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="text-sm font-medium">ID</label>
              <Input placeholder="e.g. codebase-health" value={id} onChange={(e) => setId(e.target.value)} />
            </div>
            <div>
              <label className="text-sm font-medium">Name</label>
              <Input placeholder="e.g. Codebase Health Check" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
          </div>
          <div>
            <label className="text-sm font-medium">Check prompt (body)</label>
            <textarea
              className="w-full min-h-[100px] rounded-md border bg-background px-3 py-2 text-sm resize-y"
              placeholder="What should the agent check? (Markdown supported)"
              value={body}
              onChange={(e) => setBody(e.target.value)}
            />
          </div>
          <div className="grid grid-cols-3 gap-3">
            <div>
              <label className="text-sm font-medium">Schedule (cron)</label>
              <Input placeholder="0 9 * * 1-5" value={schedule} onChange={(e) => setSchedule(e.target.value)} />
            </div>
            <div>
              <label className="text-sm font-medium">Priority</label>
              <Input type="number" min="1" max="10" value={priority} onChange={(e) => setPriority(e.target.value)} />
            </div>
            <div>
              <label className="text-sm font-medium">Tags (comma-sep)</label>
              <Input placeholder="monitoring, health" value={tags} onChange={(e) => setTags(e.target.value)} />
            </div>
          </div>
          {error && <p className="text-sm text-destructive">{error}</p>}
          <Button className="w-full" onClick={handleCreate} disabled={!id.trim() || !name.trim() || !body.trim() || create.isPending}>
            {create.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function EditHeartbeatDialog({ check, onDone }: { check: { id: string; name: string; enabled: boolean; schedule: string | null; priority: number; tags: string[] }; onDone: () => void }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(check.name);
  const [body, setBody] = useState("");
  const [schedule, setSchedule] = useState(check.schedule ?? "");
  const [priority, setPriority] = useState(String(check.priority));
  const [enabled, setEnabled] = useState(check.enabled);
  const [error, setError] = useState("");
  const { data: detail } = useHeartbeatCheckDetail(open ? check.id : undefined);
  const update = useUpdateHeartbeatCheck();

  if (detail?.body && !body) {
    setBody(detail.body);
  }

  const handleSave = () => {
    setError("");
    update.mutate(
      {
        id: check.id,
        data: {
          name: name !== check.name ? name : undefined,
          body: body !== (detail?.body ?? "") ? body : undefined,
          schedule: schedule !== (check.schedule ?? "") ? schedule || undefined : undefined,
          priority: parseInt(priority, 10) !== check.priority ? parseInt(priority, 10) : undefined,
          enabled: enabled !== check.enabled ? enabled : undefined,
        },
      },
      {
        onSuccess: () => { setOpen(false); onDone(); },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm" variant="ghost" className="h-7 text-xs"><Pencil className="h-3 w-3" /></Button>
      </DialogTrigger>
      <DialogContent className="max-w-lg">
        <DialogHeader><DialogTitle>Edit Heartbeat Check</DialogTitle></DialogHeader>
        <div className="space-y-3">
          <div>
            <label className="text-sm font-medium">Name</label>
            <Input value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div>
            <label className="text-sm font-medium">Check prompt (body)</label>
            <textarea
              className="w-full min-h-[80px] rounded-md border bg-background px-3 py-2 text-sm resize-y"
              value={body}
              onChange={(e) => setBody(e.target.value)}
              placeholder={detail ? undefined : "Loading..."}
            />
          </div>
          <div>
            <label className="text-sm font-medium">Schedule (cron)</label>
            <Input value={schedule} onChange={(e) => setSchedule(e.target.value)} placeholder="Inherits default if empty" />
          </div>
          <div className="flex items-center gap-4">
            <div>
              <label className="text-sm font-medium">Priority</label>
              <Input type="number" min="1" max="10" value={priority} onChange={(e) => setPriority(e.target.value)} className="w-20" />
            </div>
            <div>
              <label className="text-sm font-medium">Enabled</label>
              <Button size="sm" variant={enabled ? "default" : "outline"} onClick={() => setEnabled(!enabled)} className="ml-2">
                {enabled ? "Active" : "Disabled"}
              </Button>
            </div>
          </div>
          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
          <Button onClick={handleSave} disabled={update.isPending}>
            {update.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function HeartbeatHistoryDialog() {
  const [open, setOpen] = useState(false);
  const { data, isLoading } = useHeartbeatHistory();
  const history = open ? (data?.history ?? []) : [];

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm" variant="outline"><History className="h-3.5 w-3.5 mr-1" />History</Button>
      </DialogTrigger>
      <DialogContent className="max-w-2xl">
        <DialogHeader><DialogTitle>Heartbeat History</DialogTitle></DialogHeader>
        {isLoading ? (
          <div className="flex justify-center py-8"><Loader2 className="h-5 w-5 animate-spin" /></div>
        ) : history.length === 0 ? (
          <p className="text-sm text-muted-foreground py-8 text-center">No history yet.</p>
        ) : (
          <div className="max-h-80 overflow-y-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Check</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Time</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Duration</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Actionable</th>
                  <th className="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Response</th>
                </tr>
              </thead>
              <tbody>
                {history.map((r, i) => (
                  <tr key={i} className="border-b">
                    <td className="px-3 py-2 text-xs font-medium">{r.check_name}</td>
                    <td className="px-3 py-2 text-xs text-muted-foreground whitespace-nowrap">{new Date(r.timestamp).toLocaleString()}</td>
                    <td className="px-3 py-2 text-xs tabular-nums">{formatDuration(r.duration_ms)}</td>
                    <td className="px-3 py-2">
                      <Badge variant={r.actionable ? "warning" : "secondary"} className="text-[10px]">
                        {r.actionable ? "Yes" : "No"}
                      </Badge>
                    </td>
                    <td className="px-3 py-2 text-xs text-muted-foreground max-w-xs truncate">{r.response || "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function HeartbeatsTab() {
  const { data: statusData } = useHeartbeatStatus();
  const { data: checksData, isLoading, error, refetch } = useHeartbeatChecks();
  const trigger = useTriggerHeartbeatCheck();
  const del = useDeleteHeartbeatCheck();
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);

  const checks = checksData?.checks ?? [];

  if (isLoading) return <PageSkeleton />;
  if (error) return <ErrorCard message={error.message} />;

  return (
    <div className="space-y-4">
      <div className="flex justify-between items-center">
        <p className="text-sm text-muted-foreground">
          Health checks that run periodically with priority and cooldown controls.
          {statusData && (
            <span className="ml-2">
              Status: <Badge variant="outline">{statusData.enabled ? "Active" : "Inactive"}</Badge>
            </span>
          )}
        </p>
        <div className="flex gap-2">
          <HeartbeatHistoryDialog />
          <CreateHeartbeatDialog onCreated={() => refetch()} />
        </div>
      </div>
      {checks.length === 0 ? (
        <EmptyState icon={Heart} title="No heartbeat checks" description="Create a heartbeat check to monitor your systems." />
      ) : (
        <Card>
          <CardContent className="p-0">
            <table className="w-full">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Name</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Schedule</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Priority</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Status</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Tags</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground"></th>
                </tr>
              </thead>
              <tbody>
                {checks.map((c) => (
                  <tr key={c.id} className="border-b hover:bg-muted/50">
                    <td className="px-4 py-3 text-sm font-medium">{c.name}</td>
                    <td className="px-4 py-3 font-mono text-xs text-muted-foreground">{c.schedule ?? "default"}</td>
                    <td className="px-4 py-3 text-sm">{c.priority}</td>
                    <td className="px-4 py-3">
                      <Badge variant={c.enabled ? "secondary" : "outline"}>{c.enabled ? "Enabled" : "Disabled"}</Badge>
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex gap-1 flex-wrap">
                        {(c.tags ?? []).map((t) => (
                          <Badge key={t} variant="outline" className="text-xs">{t}</Badge>
                        ))}
                      </div>
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex gap-1">
                        <EditHeartbeatDialog check={c} onDone={() => refetch()} />
                        <Button
                          size="sm"
                          variant="ghost"
                          className="h-7 text-xs"
                          onClick={() => trigger.mutate(c.id, { onSuccess: () => refetch() })}
                          disabled={trigger.isPending}
                        >
                          <Play className="h-3 w-3" />
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          className="h-7 text-xs text-destructive"
                          onClick={() => {
                            if (deleteTarget !== c.id) { setDeleteTarget(c.id); return; }
                            del.mutate(c.id, { onSuccess: () => { setDeleteTarget(null); refetch(); } });
                          }}
                        >
                          {deleteTarget === c.id ? "Confirm?" : <Trash2 className="h-3 w-3" />}
                        </Button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

// ── Reminders Tab ────────────────────────────────────────────────────────────

function CreateReminderDialog() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [message, setMessage] = useState("");
  const [datetime, setDatetime] = useState("");
  const [error, setError] = useState("");
  const create = useCreateJob();

  const handleCreate = () => {
    setError("");
    if (!name.trim() || !message.trim() || !datetime.trim()) return;
    create.mutate(
      { name, message, schedule: { kind: "at", at: new Date(datetime).toISOString() } },
      {
        onSuccess: () => {
          setOpen(false);
          setName(""); setMessage(""); setDatetime("");
        },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm"><Plus className="mr-1.5 h-4 w-4" />Create Reminder</Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader><DialogTitle>Create Reminder</DialogTitle></DialogHeader>
        <div className="space-y-3">
          <div>
            <label className="text-sm font-medium">Name</label>
            <Input placeholder="e.g. Follow up on PR" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div>
            <label className="text-sm font-medium">When</label>
            <Input type="datetime-local" value={datetime} onChange={(e) => setDatetime(e.target.value)} />
          </div>
          <div>
            <label className="text-sm font-medium">Message</label>
            <Input placeholder="What to be reminded about?" value={message} onChange={(e) => setMessage(e.target.value)} />
          </div>
          {error && <p className="text-sm text-destructive">{error}</p>}
          <Button className="w-full" onClick={handleCreate} disabled={!name.trim() || !message.trim() || !datetime.trim() || create.isPending}>
            {create.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Create"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function RemindersTab() {
  const { data, isLoading, error, refetch } = useScheduledJobs();
  const { canMutate } = useScheduleAccess();
  const reminders = (data?.jobs ?? []).filter((j) => j.is_reminder);

  if (isLoading) return <PageSkeleton />;
  if (error) return <ErrorCard message={error.message} />;

  return (
    <div className="space-y-4">
      <div className="flex justify-between items-center">
        <p className="text-sm text-muted-foreground">One-time scheduled messages. Created by agents or manually.</p>
        <CreateReminderDialog />
      </div>
      {reminders.length === 0 ? (
        <EmptyState icon={Bell} title="No reminders" description="Reminders fire once at a scheduled time." />
      ) : (
        <Card>
          <CardContent className="p-0">
            <table className="w-full">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Name</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Owner</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Scheduled For</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Status</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Next Run</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">Runs</th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground"></th>
                </tr>
              </thead>
              <tbody>
                {reminders.map((r) => (
                  <JobRow key={r.id} job={r} onTriggered={() => refetch()} canMutate={canMutate(r)} />
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

// ── Error Card ───────────────────────────────────────────────────────────────

function ErrorCard({ message }: { message: string }) {
  return (
    <Card>
      <CardContent className="flex items-center gap-2 p-6 text-destructive">
        <AlertCircle className="h-5 w-5" />
        <p>Failed to load: {message}</p>
      </CardContent>
    </Card>
  );
}

function SchedulerHistoryTab() {
  const sevenDaysAgo = useMemo(
    () => new Date(Date.now() - 7 * 24 * 60 * 60 * 1000).toISOString(),
    [],
  );
  const { data, isLoading, error } = useScheduledJobHistory({
    start: sevenDaysAgo,
    limit: 200,
  });

  const records = data?.records ?? [];

  if (isLoading) return <PageSkeleton />;
  if (error)
    return (
      <Card>
        <CardContent className="flex items-center gap-2 p-6 text-destructive">
          <AlertCircle className="h-5 w-5" />
          <p>Failed to load job history: {error.message}</p>
        </CardContent>
      </Card>
    );

  const totalCost = records.reduce((s, r) => s + r.cost_usd, 0);

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <p className="text-sm text-muted-foreground">
          Run-by-run history for scheduled jobs over the last 7 days. Cost is
          the LLM spend attributed to each run via its task id; tool/shell/
          monitor jobs that never call an LLM correctly read $0.
        </p>
        <span className="text-sm text-muted-foreground">
          {records.length.toLocaleString()} runs · {formatCostScaled(totalCost)}
        </span>
      </div>

      {records.length === 0 ? (
        <EmptyState
          icon={History}
          title="No executions in last 7 days"
          description="Job runs will appear here as they complete."
        />
      ) : (
        <Card>
          <CardContent className="p-0">
            <table className="w-full text-sm" aria-label="Scheduled job history">
              <thead>
                <tr className="border-b bg-muted/50">
                  {["Time", "Agent", "User", "Description", "Type", "Duration", "Cost", "Status"].map(
                    (h) => (
                      <th
                        key={h}
                        className="px-4 py-2 text-left text-xs font-medium text-muted-foreground whitespace-nowrap"
                      >
                        {h}
                      </th>
                    ),
                  )}
                </tr>
              </thead>
              <tbody>
                {records.map((r) => (
                  <tr key={r.id} className="border-b hover:bg-muted/50">
                    <td className="px-4 py-2 text-xs text-muted-foreground whitespace-nowrap">
                      {new Date(r.completed_at).toLocaleString(undefined, {
                        month: "short",
                        day: "numeric",
                        hour: "2-digit",
                        minute: "2-digit",
                      })}
                    </td>
                    <td className="px-4 py-2 text-xs whitespace-nowrap">
                      <Badge variant="outline" className="text-xs">
                        {r.agent_display_name || r.agent_id}
                      </Badge>
                    </td>
                    <td className="px-4 py-2 text-xs whitespace-nowrap">
                      <PrincipalPill
                        principal={
                          r.user_id
                            ? {
                                id: r.user_id,
                                display_name: r.user_display_name,
                                kind: r.user_kind ?? "user",
                              }
                            : null
                        }
                      />
                    </td>
                    <td className="px-4 py-2 max-w-[260px]">
                      <span
                        className="text-xs text-muted-foreground truncate block"
                        title={r.description ?? r.task_type ?? undefined}
                      >
                        {r.description ?? r.task_type ?? "—"}
                      </span>
                    </td>
                    <td className="px-4 py-2 text-xs text-muted-foreground whitespace-nowrap">
                      {r.task_type ?? "—"}
                    </td>
                    <td className="px-4 py-2 text-xs text-muted-foreground whitespace-nowrap">
                      {formatDuration(r.duration_ms)}
                    </td>
                    <td className="px-4 py-2 text-xs font-mono whitespace-nowrap">
                      {r.cost_usd > 0 ? (
                        <span className="text-amber-600 dark:text-amber-400">
                          {formatCostScaled(r.cost_usd)}
                        </span>
                      ) : (
                        <span className="text-muted-foreground">—</span>
                      )}
                    </td>
                    <td className="px-4 py-2 whitespace-nowrap">
                      {r.success ? (
                        <Badge variant="outline" className="text-xs text-green-600 border-green-300">
                          ok
                        </Badge>
                      ) : (
                        <Badge variant="destructive" className="text-xs">
                          {r.error_type ?? "fail"}
                        </Badge>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

export default function SchedulerPage() {
  const { isAdmin } = useScheduleAccess();
  const hasHeartbeatCapability = useHasCapability("heartbeat_manage");
  const canManageHeartbeats = isAdmin || hasHeartbeatCapability;
  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-2xl font-bold">Scheduler</h1>
        <p className="text-sm text-muted-foreground">
          {canManageHeartbeats
            ? "Manage scheduled jobs, heartbeat checks, and reminders."
            : "Manage your scheduled jobs and reminders."}
        </p>
      </div>

      <Tabs defaultValue="schedules">
        <TabsList>
          <TabsTrigger value="schedules" className="gap-1.5">
            <Clock className="h-3.5 w-3.5" />Schedules
          </TabsTrigger>
          {canManageHeartbeats && (
            <TabsTrigger value="heartbeats" className="gap-1.5">
              <Heart className="h-3.5 w-3.5" />Heartbeats
            </TabsTrigger>
          )}
          <TabsTrigger value="reminders" className="gap-1.5">
            <Bell className="h-3.5 w-3.5" />Reminders
          </TabsTrigger>
          {isAdmin && (
            <TabsTrigger value="history" className="gap-1.5">
              <History className="h-3.5 w-3.5" />
              History
            </TabsTrigger>
          )}
        </TabsList>
        <TabsContent value="schedules" className="mt-4"><SchedulesTab /></TabsContent>
        {canManageHeartbeats && <TabsContent value="heartbeats" className="mt-4"><HeartbeatsTab /></TabsContent>}
        <TabsContent value="reminders" className="mt-4"><RemindersTab /></TabsContent>
        {isAdmin && <TabsContent value="history" className="mt-4"><SchedulerHistoryTab /></TabsContent>}
      </Tabs>
    </div>
  );
}
