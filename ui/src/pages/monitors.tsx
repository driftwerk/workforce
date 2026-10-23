import { useCallback, useState } from "react";
import { useNavigate } from "react-router";
import { ChevronDown, Eye, Hash, Plus, Radar } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useMonitors, useSaveMonitor } from "@/hooks/use-monitors";
import { MonitorCard } from "@/components/monitors/monitor-card";
import { DeleteMonitorDialog } from "@/components/monitors/delete-monitor-dialog";
import { ShareDialog } from "@/components/sharing/share-dialog";
import { TransferDialog } from "@/components/sharing/transfer-dialog";
import { monitorsApi, type MonitorDef, type MonitorKind } from "@/lib/api/monitors";
import { useAuthStore } from "@/stores/auth-store";

type OwnerFilter = "all" | "mine" | "global";

export default function MonitorsPage() {
  const { data, isLoading, refetch } = useMonitors();
  const navigate = useNavigate();
  const saveMutation = useSaveMonitor();

  const [deleteMon, setDeleteMon] = useState<MonitorDef | null>(null);
  const [shareMon, setShareMon] = useState<MonitorDef | null>(null);
  const [transferMon, setTransferMon] = useState<MonitorDef | null>(null);
  const [ownerFilter, setOwnerFilter] = useState<OwnerFilter>("all");

  const myId = useAuthStore((s) => s.identity?.id);
  const allMonitors = data?.monitors ?? [];
  const monitors = allMonitors.filter((m) => {
    if (ownerFilter === "mine") return !!myId && m.owner === myId;
    if (ownerFilter === "global") return m.visibility === "global";
    return true;
  });

  const handleDuplicate = useCallback(
    async (m: MonitorDef) => {
      try {
        const source = await monitorsApi.getSource(m.id, m.owner);
        const newId = `${m.id}-copy`;
        const newToml = (source as string).replace(/id\s*=\s*"[^"]*"/, `id = "${newId}"`);
        saveMutation.mutate(
          { id: newId, toml: newToml },
          {
            onSuccess: () => { refetch(); navigate(`/monitors/${newId}`); },
            onError: (e) => alert(`Duplicate failed: ${e instanceof Error ? e.message : String(e)}`),
          },
        );
      } catch (err) {
        alert(`Duplicate failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
    [saveMutation, refetch, navigate],
  );

  const handleExportSingle = useCallback(async (m: MonitorDef) => {
    try {
      const source = await monitorsApi.getSource(m.id, m.owner);
      const blob = new Blob([source as string], { type: "application/toml" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${m.id}.toml`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      alert(`Export failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, []);

  const handleNew = useCallback(
    (kind: MonitorKind) => navigate(`/monitors/new?kind=${kind}`),
    [navigate],
  );

  if (isLoading) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-2xl font-bold">Monitors</h1>
          <p className="text-muted-foreground">
            Durably watch a chat channel, or anything a tool reads — a mailbox, a query, a
            build, a page — and act on each matching message or change: run a workflow or a
            skill, post a notification, or hand it to an agent. Survives restarts, and catches
            up when it comes back.
          </p>
        </div>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button disabled={saveMutation.isPending}>
              <Plus className="h-4 w-4 mr-1.5" />
              New Monitor
              <ChevronDown className="h-4 w-4 ml-1.5" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onClick={() => handleNew("channel")}>
              <Hash className="h-3.5 w-3.5 mr-2" />Watch a channel
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => handleNew("source")}>
              <Radar className="h-3.5 w-3.5 mr-2" />Watch a source
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>

      {allMonitors.length > 0 && (
        <div className="flex items-center border rounded-md w-fit">
          {(["all", "mine", "global"] as OwnerFilter[]).map((f, i) => (
            <Button
              key={f}
              variant={ownerFilter === f ? "secondary" : "ghost"}
              size="sm"
              className={`h-8 px-2.5 text-xs ${i === 0 ? "rounded-r-none" : i === 1 ? "rounded-none" : "rounded-l-none"}`}
              onClick={() => setOwnerFilter(f)}
            >
              {f === "all" ? "All" : f === "mine" ? "Mine" : "Global"}
            </Button>
          ))}
        </div>
      )}

      {monitors.length === 0 ? (
        <EmptyState
          icon={Eye}
          title="No monitors"
          description="Use New Monitor above to watch a channel or a source, or ask an agent to set one up with the monitor tool."
        />
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {monitors.map((m) => (
            <MonitorCard
              key={`${m.owner ?? "sys"}/${m.id}`}
              monitor={m}
              onDuplicate={handleDuplicate}
              onExport={handleExportSingle}
              onShare={setShareMon}
              onTransfer={setTransferMon}
              onDelete={setDeleteMon}
            />
          ))}
        </div>
      )}

      <DeleteMonitorDialog
        monitor={deleteMon}
        open={deleteMon !== null}
        onOpenChange={(open) => { if (!open) setDeleteMon(null); }}
        onDeleted={() => refetch()}
      />

      {shareMon && (
        <ShareDialog
          kind="monitors"
          id={shareMon.id}
          owner={shareMon.owner}
          name={shareMon.name}
          open={shareMon !== null}
          onOpenChange={(open) => { if (!open) setShareMon(null); }}
        />
      )}
      {transferMon && (
        <TransferDialog
          kind="monitors"
          id={transferMon.id}
          owner={transferMon.owner}
          name={transferMon.name}
          open={transferMon !== null}
          onOpenChange={(open) => { if (!open) setTransferMon(null); }}
        />
      )}
    </div>
  );
}
