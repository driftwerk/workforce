import { useState } from "react";
import { Link, useParams, useNavigate, useSearchParams } from "react-router";
import { ArrowLeft, Save, Loader2, KeyRound, Plus, X, Smartphone, Eraser } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  DateRangePicker,
  defaultRange,
  type DateRange,
} from "@/components/ui/date-range-picker";
import { DataTable, type Column } from "@/components/ui/data-table";
import { usePairedDevices } from "@/hooks/use-pairing";
import type { PairedDevice } from "@/components/pairing/device-list";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useIdentity, useUpdateIdentity, useSetPassword } from "@/hooks/use-identities";
import { useEraseKnowHow } from "@/hooks/use-agent-know-how";
import { apiErrorMessage } from "@/lib/api-error";
import { useAllRoles } from "@/hooks/use-permissions";
import { useIdentityStats, useIdentityTasks } from "@/hooks/use-stats";
import type { TaskHistoryEntry, ToolBreakdown } from "@/lib/api/stats";
import { TraceDrawer } from "@/components/stats/trace-drawer";
import { StatCard } from "@/components/stats/stat-card";
import { TokenUsageChart } from "@/components/stats/token-usage-chart";
import { CostBarChart } from "@/components/stats/cost-bar-chart";
import { formatCost, formatDuration } from "@/lib/utils";
import { IdentityLifecycle } from "@/components/identity-lifecycle";

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function SetPasswordDialog({ userId }: { userId: string }) {
  const [open, setOpen] = useState(false);
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState("");
  const [success, setSuccess] = useState(false);
  const setPw = useSetPassword();

  const handleSubmit = () => {
    setError("");
    setSuccess(false);
    if (password.length < 8) {
      setError("Password must be at least 8 characters");
      return;
    }
    if (password !== confirm) {
      setError("Passwords do not match");
      return;
    }
    setPw.mutate(
      { id: userId, password },
      {
        onSuccess: () => {
          setSuccess(true);
          setPassword("");
          setConfirm("");
        },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); setSuccess(false); setError(""); }}>
      <DialogTrigger asChild>
        <Button size="sm" variant="outline">
          <KeyRound className="mr-1.5 h-4 w-4" />
          Set Password
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Set Password</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <div>
            <label htmlFor="new-pw" className="text-sm font-medium">
              New password
            </label>
            <Input
              id="new-pw"
              type="password"
              placeholder="Minimum 8 characters"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </div>
          <div>
            <label htmlFor="confirm-pw" className="text-sm font-medium">
              Confirm password
            </label>
            <Input
              id="confirm-pw"
              type="password"
              placeholder="Repeat password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
          </div>
          {error && (
            <p className="text-sm text-destructive" role="alert">
              {error}
            </p>
          )}
          {success && (
            <p className="text-sm text-green-600" role="alert">
              Password set successfully
            </p>
          )}
          <Button
            className="w-full"
            onClick={handleSubmit}
            disabled={setPw.isPending || !password || !confirm}
          >
            {setPw.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              "Set Password"
            )}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function EraseKnowHowDialog({ userId }: { userId: string }) {
  const [open, setOpen] = useState(false);
  const erase = useEraseKnowHow();

  return (
    <Dialog open={open} onOpenChange={(v) => { setOpen(v); erase.reset(); }}>
      <DialogTrigger asChild>
        <Button size="sm" variant="outline">
          <Eraser className="mr-1.5 h-4 w-4" />
          Erase Know-how
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Erase know-how contributions</DialogTitle>
          <DialogDescription>
            Removes everything this person contributed to every agent's know-how. Entries only
            they produced are deleted; shared ones lose their confirmation.
          </DialogDescription>
        </DialogHeader>
        {erase.error && (
          <p className="text-sm text-destructive" role="alert">
            {apiErrorMessage(erase.error)}
          </p>
        )}
        {erase.data && (
          <p className="text-sm text-green-600" role="alert">
            Erased from {erase.data.entries_touched} entries
          </p>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => setOpen(false)}>
            {erase.data ? "Close" : "Cancel"}
          </Button>
          <Button
            variant="destructive"
            disabled={erase.isPending || !!erase.data}
            onClick={() => erase.mutate(userId)}
          >
            {erase.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : "Erase"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export default function UserDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const { data: user, isLoading, error } = useIdentity(id!);
  const update = useUpdateIdentity();
  const [editName, setEditName] = useState<string | null>(null);
  const [editEmail, setEditEmail] = useState<string | null>(null);
  const [editRole, setEditRole] = useState<string | null>(null);
  const [newPlatform, setNewPlatform] = useState("");
  const [newPlatformId, setNewPlatformId] = useState("");

  if (isLoading) return <PageSkeleton />;

  if (error || !user) {
    return (
      <div className="p-6">
        <div className="text-destructive">
          {error ? `Failed to load user: ${error.message}` : "User not found."}
        </div>
      </div>
    );
  }

  const isEditing =
    editName !== null || editEmail !== null || editRole !== null;

  const handleSave = () => {
    const changes: Record<string, string> = {};
    if (editName !== null && editName !== user.display_name)
      changes.display_name = editName;
    if (editEmail !== null && editEmail !== (user.email ?? ""))
      changes.email = editEmail;
    if (editRole !== null && editRole !== user.role) changes.role = editRole;
    if (Object.keys(changes).length === 0) {
      setEditName(null);
      setEditEmail(null);
      setEditRole(null);
      return;
    }
    update.mutate(
      { id: user.id, ...changes },
      {
        onSuccess: () => {
          setEditName(null);
          setEditEmail(null);
          setEditRole(null);
        },
      },
    );
  };

  const linkedEntries = Object.entries(user.linked_ids ?? {});

  const handleLinkAdd = () => {
    if (!newPlatform.trim() || !newPlatformId.trim()) return;
    const updated = { ...user.linked_ids, [newPlatform.trim()]: newPlatformId.trim() };
    update.mutate(
      { id: user.id, linked_ids: updated },
      { onSuccess: () => { setNewPlatform(""); setNewPlatformId(""); } },
    );
  };

  const handleLinkRemove = (platform: string) => {
    const updated = { ...user.linked_ids };
    delete updated[platform];
    update.mutate({ id: user.id, linked_ids: updated });
  };

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-3">
        <Button
          variant="ghost"
          size="icon"
          onClick={() => navigate("/users")}
          aria-label="Back to users"
        >
          <ArrowLeft className="h-4 w-4" />
        </Button>
        <div className="flex-1">
          <h1 className="text-2xl font-bold">{user.display_name}</h1>
          <p className="text-muted-foreground font-mono text-sm">{user.id}</p>
        </div>
        <div className="flex gap-2">
          <SetPasswordDialog userId={user.id} />
          <EraseKnowHowDialog userId={user.id} />
          {isEditing && (
            <Button size="sm" onClick={handleSave} disabled={update.isPending}>
              {update.isPending ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <>
                  <Save className="mr-1.5 h-4 w-4" />
                  Save
                </>
              )}
            </Button>
          )}
        </div>
      </div>

      <UserDetailTabs
        userId={user.id}
        profile={
          <div className="space-y-6">
            <div className="grid gap-4 sm:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Display Name
            </CardTitle>
          </CardHeader>
          <CardContent>
            <Input
              value={editName ?? user.display_name}
              onChange={(e) => setEditName(e.target.value)}
              aria-label="Display name"
            />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Email
            </CardTitle>
          </CardHeader>
          <CardContent>
            <Input
              type="email"
              value={editEmail ?? user.email ?? ""}
              onChange={(e) => setEditEmail(e.target.value)}
              placeholder="Set email for login access"
              aria-label="Email"
            />
          </CardContent>
        </Card>

        <RoleCard
          value={editRole ?? user.role}
          onChange={setEditRole}
        />

        <Card>
          <CardHeader>
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Status
            </CardTitle>
          </CardHeader>
          <CardContent>
            <Badge variant="outline">
              {(user.status ?? "active").replace(/_/g, " ")}
            </Badge>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Provisioning
            </CardTitle>
          </CardHeader>
          <CardContent>
            <Badge variant="secondary">
              {(user.provisioning_source ?? "manual").replace(/_/g, " ")}
            </Badge>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm font-medium text-muted-foreground">
              Created
            </CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-sm">
              {user.created_at
                ? new Date(user.created_at).toLocaleDateString()
                : "—"}
            </p>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Linked Identities</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          {linkedEntries.map(([platform, platformId]) => (
            <div
              key={platform}
              className="flex items-center justify-between rounded-md border px-3 py-2"
            >
              <span className="text-sm font-medium capitalize">{platform}</span>
              <div className="flex items-center gap-2">
                <span className="font-mono text-sm text-muted-foreground">
                  {platformId}
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6"
                  onClick={() => handleLinkRemove(platform)}
                  disabled={update.isPending}
                  aria-label={`Remove ${platform} link`}
                >
                  <X className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
          ))}
          <DeviceLinkSelector
            existingIds={linkedEntries.map(([, id]) => id)}
            onLink={(deviceId, _name, kind) => {
              // One key per device, so a person keeps every device they own.
              const updated = { ...user.linked_ids, [`${kind}:${deviceId}`]: deviceId };
              update.mutate({ id: user.id, linked_ids: updated });
            }}
            disabled={update.isPending}
          />
          <div className="flex gap-2">
            <Input
              placeholder="Platform (e.g. ios_device)"
              value={newPlatform}
              onChange={(e) => setNewPlatform(e.target.value)}
              className="flex-1"
              aria-label="Platform name"
            />
            <Input
              placeholder="ID"
              value={newPlatformId}
              onChange={(e) => setNewPlatformId(e.target.value)}
              className="flex-1"
              aria-label="Platform ID"
            />
            <Button
              size="icon"
              variant="outline"
              onClick={handleLinkAdd}
              disabled={!newPlatform.trim() || !newPlatformId.trim() || update.isPending}
              aria-label="Add link"
            >
              <Plus className="h-4 w-4" />
            </Button>
          </div>
        </CardContent>
      </Card>
          </div>
        }
      />
    </div>
  );
}

function RoleCard({
  value,
  onChange,
}: {
  value: string;
  onChange: (role: string) => void;
}) {
  const roles = useAllRoles();
  const options = roles.data?.roles ?? [];
  // Ensure the user's current role is selectable even if the catalogue hasn't
  // loaded yet (or the stored role is unknown), so the value never silently
  // resets on save.
  const known = options.some((r) => r.key === value);

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-sm font-medium text-muted-foreground">
          Role
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2">
        <select
          className="w-full rounded-md border px-3 py-2 text-sm bg-background"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          aria-label="Role"
        >
          {!known && <option value={value}>{value}</option>}
          {options.map((r) => (
            <option key={r.key} value={r.key}>
              {r.label}
            </option>
          ))}
        </select>
        <p className="text-xs text-muted-foreground">
          A role&rsquo;s capabilities are defined on the{" "}
          <Link to="/permissions" className="underline">
            Permissions
          </Link>{" "}
          page.
        </p>
      </CardContent>
    </Card>
  );
}

function UserDetailTabs({
  userId,
  profile,
}: {
  userId: string;
  profile: React.ReactNode;
}) {
  const [search, setSearch] = useSearchParams();
  const asked = search.get("tab");
  const tab = asked === "usage" || asked === "lifecycle" ? asked : "profile";
  return (
    <Tabs
      value={tab}
      onValueChange={(v) => {
        const next = new URLSearchParams(search);
        if (v === "profile") next.delete("tab");
        else next.set("tab", v);
        setSearch(next, { replace: true });
      }}
    >
      <TabsList>
        <TabsTrigger value="profile">Profile</TabsTrigger>
        <TabsTrigger value="usage">Usage</TabsTrigger>
        <TabsTrigger value="lifecycle">Merge &amp; delete</TabsTrigger>
      </TabsList>
      <TabsContent value="profile">{profile}</TabsContent>
      <TabsContent value="usage">
        <UsageTab identityId={userId} />
      </TabsContent>
      <TabsContent value="lifecycle">
        <IdentityLifecycle identityId={userId} />
      </TabsContent>
    </Tabs>
  );
}

function UsageTab({ identityId }: { identityId: string }) {
  const [range, setRange] = useState<DateRange>(defaultRange());
  const [trace, setTrace] = useState<string | null>(null);
  const [cursor, setCursor] = useState<string | undefined>();
  const stats = useIdentityStats(identityId, range);
  const tasks = useIdentityTasks(identityId, range, { cursor, limit: 25 });

  const totals = stats.data?.totals;
  const topModels = (stats.data?.by_model ?? [])
    .slice(0, 8)
    .map((m) => ({ label: m.group_key, cost: m.total_cost }));

  const taskCols: Column<TaskHistoryEntry>[] = [
    { key: "when", header: "When", cell: (r) => new Date(r.completed_at).toLocaleString() },
    { key: "agent", header: "Agent", cell: (r) => r.agent_id },
    { key: "model", header: "Model", cell: (r) => <span className="font-mono text-xs">{r.model}</span> },
    { key: "rounds", header: "Rounds", align: "right", cell: (r) => r.rounds },
    { key: "tools", header: "Tools", align: "right", cell: (r) => r.tool_call_count },
    { key: "cost", header: "Cost", align: "right", cell: (r) => formatCost(r.cost_usd) },
    {
      key: "ok",
      header: "Outcome",
      cell: (r) => (r.success ? <Badge variant="default">ok</Badge> : <Badge variant="destructive">fail</Badge>),
    },
  ];

  const toolCols: Column<ToolBreakdown>[] = [
    { key: "tool", header: "Tool", cell: (r) => <span className="font-mono">{r.tool_name}</span> },
    { key: "calls", header: "Calls", align: "right", cell: (r) => r.invocations.toLocaleString() },
    { key: "success", header: "Success", align: "right", cell: (r) => `${r.success_rate_pct.toFixed(1)}%` },
    { key: "duration", header: "Avg time", align: "right", cell: (r) => formatDuration(r.avg_duration_ms) },
  ];

  return (
    <div className="space-y-6 mt-4">
      <div className="flex justify-end">
        <DateRangePicker value={range} onChange={setRange} />
      </div>

      <div className="grid gap-4 md:grid-cols-4">
        <StatCard label="Cost" value={formatCost(totals?.cost_usd ?? 0)} />
        <StatCard label="Tasks" value={(totals?.task_count ?? 0).toLocaleString()} />
        <StatCard label="Input tokens" value={formatTokens(totals?.input_tokens ?? 0)} />
        <StatCard label="Output tokens" value={formatTokens(totals?.output_tokens ?? 0)} />
      </div>

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Token usage over time</CardTitle>
        </CardHeader>
        <CardContent>
          <TokenUsageChart data={stats.data?.history ?? []} gradientId="user-usage" />
        </CardContent>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <CostBarChart title="Top models" rows={topModels} />
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="text-base">Tools used</CardTitle>
          </CardHeader>
          <CardContent>
            <DataTable<ToolBreakdown>
              columns={toolCols}
              rows={stats.data?.tool_breakdown ?? []}
              rowKey={(r) => r.tool_name}
              empty="No tool calls in range."
            />
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader className="pb-2">
          <CardTitle className="text-base">Recent tasks</CardTitle>
        </CardHeader>
        <CardContent>
          <DataTable<TaskHistoryEntry>
            columns={taskCols}
            rows={tasks.data?.entries ?? []}
            rowKey={(r) => r.id}
            isLoading={tasks.isLoading}
            onRowClick={(r) => r.task_id && setTrace(r.task_id)}
            footer={
              tasks.data?.next_cursor && (
                <button
                  onClick={() => setCursor(tasks.data.next_cursor!)}
                  className="rounded-md px-3 py-1 hover:bg-muted"
                >
                  Load more →
                </button>
              )
            }
          />
        </CardContent>
      </Card>

      <TraceDrawer taskId={trace} onClose={() => setTrace(null)} />
    </div>
  );
}

function DeviceLinkSelector({
  existingIds,
  onLink,
  disabled,
}: {
  existingIds: string[];
  onLink: (deviceId: string, deviceName: string, kind: string) => void;
  disabled: boolean;
}) {
  const { data } = usePairedDevices();
  const [kind, setKind] = useState("ios_device");
  // Devices nobody owns yet; one that belongs to someone is moved, not linked.
  const devices = ((data?.devices ?? []) as PairedDevice[]).filter(
    (d) => !d.revoked && !d.owner && !existingIds.includes(d.id),
  );

  if (devices.length === 0) return null;

  return (
    <div className="flex flex-wrap items-center gap-2">
      <select
        className="h-8 rounded-md border bg-background px-2 text-xs"
        value={kind}
        onChange={(e) => setKind(e.target.value)}
        aria-label="Device kind"
      >
        <option value="ios_device">iPhone or iPad</option>
        <option value="desktop_device">Desk</option>
        <option value="voice_device">Voice device</option>
      </select>
      {devices.map((device) => (
        <Button
          key={device.id}
          variant="outline"
          size="sm"
          className="h-8 text-xs"
          disabled={disabled}
          onClick={() => onLink(device.id, device.device_name, kind)}
        >
          <Smartphone className="mr-1.5 h-3.5 w-3.5" />
          {device.device_name || device.id.slice(0, 8)}
        </Button>
      ))}
    </div>
  );
}
