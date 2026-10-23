import { useCallback, useState } from "react";
import { useNavigate } from "react-router";
import { BookMarked, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { usePlaybooks, useSavePlaybook } from "@/hooks/use-playbooks";
import { PlaybookCard } from "@/components/playbooks/playbook-card";
import { DeletePlaybookDialog } from "@/components/playbooks/delete-playbook-dialog";
import { ShareDialog } from "@/components/sharing/share-dialog";
import { TransferDialog } from "@/components/sharing/transfer-dialog";
import { playbooksApi, type PlaybookDef } from "@/lib/api/playbooks";
import { useAuthStore } from "@/stores/auth-store";

type OwnerFilter = "all" | "mine" | "global";

export default function PlaybooksPage() {
  const { data, isLoading, refetch } = usePlaybooks();
  const navigate = useNavigate();
  const saveMutation = useSavePlaybook();

  const [deletePb, setDeletePb] = useState<PlaybookDef | null>(null);
  const [sharePb, setSharePb] = useState<PlaybookDef | null>(null);
  const [transferPb, setTransferPb] = useState<PlaybookDef | null>(null);
  const [ownerFilter, setOwnerFilter] = useState<OwnerFilter>("all");

  const myId = useAuthStore((s) => s.identity?.id);
  const allPlaybooks = data?.playbooks ?? [];
  const playbooks = allPlaybooks.filter((pb) => {
    if (ownerFilter === "mine") return !!myId && pb.owner === myId;
    if (ownerFilter === "global") return pb.visibility === "global";
    return true;
  });

  const handleDuplicate = useCallback(
    async (pb: PlaybookDef) => {
      try {
        const source = await playbooksApi.getSource(pb.id, pb.owner);
        const newId = `${pb.id}-copy`;
        const newToml = (source as string).replace(/id\s*=\s*"[^"]*"/, `id = "${newId}"`);
        saveMutation.mutate(
          { id: newId, toml: newToml },
          { onSuccess: () => { refetch(); navigate(`/playbooks/${newId}`); } },
        );
      } catch (err) {
        alert(`Duplicate failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
    [saveMutation, refetch, navigate],
  );

  const handleExportSingle = useCallback(async (pb: PlaybookDef) => {
    try {
      const source = await playbooksApi.getSource(pb.id, pb.owner);
      const blob = new Blob([source as string], { type: "application/toml" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${pb.id}.toml`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      alert(`Export failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, []);

  const handleNew = useCallback(() => navigate("/playbooks/new"), [navigate]);

  if (isLoading) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-2xl font-bold">Playbooks</h1>
          <p className="text-muted-foreground">
            Interactive, human-in-the-loop flows. Users run them in chat with
            <code className="mx-1 text-xs">cmd: playbook start &lt;id&gt;</code>.
          </p>
        </div>
        <Button onClick={handleNew} disabled={saveMutation.isPending}>
          <Plus className="h-4 w-4 mr-1.5" />
          New Playbook
        </Button>
      </div>

      {allPlaybooks.length > 0 && (
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

      {playbooks.length === 0 ? (
        <EmptyState
          icon={BookMarked}
          title="No playbooks"
          description="Create a playbook definition, or save one from chat with the playbook tool."
          action={{ label: "New Playbook", onClick: handleNew }}
        />
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {playbooks.map((pb) => (
            <PlaybookCard
              key={`${pb.owner ?? "sys"}/${pb.id}`}
              playbook={pb}
              onDuplicate={handleDuplicate}
              onExport={handleExportSingle}
              onShare={setSharePb}
              onTransfer={setTransferPb}
              onDelete={setDeletePb}
            />
          ))}
        </div>
      )}

      <DeletePlaybookDialog
        playbook={deletePb}
        open={deletePb !== null}
        onOpenChange={(open) => { if (!open) setDeletePb(null); }}
        onDeleted={() => refetch()}
      />
      {sharePb && (
        <ShareDialog
          kind="playbooks"
          id={sharePb.id}
          owner={sharePb.owner}
          name={sharePb.name}
          open={sharePb !== null}
          onOpenChange={(open) => { if (!open) setSharePb(null); }}
        />
      )}
      {transferPb && (
        <TransferDialog
          kind="playbooks"
          id={transferPb.id}
          owner={transferPb.owner}
          name={transferPb.name}
          open={transferPb !== null}
          onOpenChange={(open) => { if (!open) setTransferPb(null); }}
        />
      )}
    </div>
  );
}
