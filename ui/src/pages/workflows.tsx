import { useState, useCallback } from "react";
import { useNavigate } from "react-router";
import { GitBranch, LayoutGrid, List, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { EmptyState } from "@/components/empty-state";
import { PageSkeleton } from "@/components/loading-skeleton";
import { useWorkflows, useSaveWorkflow, useWorkflowInstances } from "@/hooks/use-workflows";
import { WorkflowCard } from "@/components/workflows/workflow-card";
import { WorkflowList } from "@/components/workflows/workflow-list";
import { TemplateGallery } from "@/components/workflows/template-gallery";
import { QuickStartWizard } from "@/components/workflows/quick-start-wizard";
import { DeleteWorkflowDialog } from "@/components/workflows/delete-workflow-dialog";
import { ShareDialog } from "@/components/sharing/share-dialog";
import { TransferDialog } from "@/components/sharing/transfer-dialog";
import { ImportExport } from "@/components/workflows/import-export";
import { InstanceTable } from "@/components/workflows/instance-table";
import { workflowsApi, type WorkflowDef } from "@/lib/api/workflows";
import { useAuthStore } from "@/stores/auth-store";

type ViewType = "grid" | "list";
type OwnerFilter = "all" | "mine" | "global";

export default function WorkflowsPage() {
  const { data, isLoading, refetch } = useWorkflows();
  const navigate = useNavigate();
  const saveMutation = useSaveWorkflow();

  const [viewType, setViewType] = useState<ViewType>("grid");
  const [ownerFilter, setOwnerFilter] = useState<OwnerFilter>("all");
  const [templateWf, setTemplateWf] = useState<WorkflowDef | null>(null);
  const [deleteWf, setDeleteWf] = useState<WorkflowDef | null>(null);
  const [shareWf, setShareWf] = useState<WorkflowDef | null>(null);
  const [transferWf, setTransferWf] = useState<WorkflowDef | null>(null);
  const [instanceStatus, setInstanceStatus] = useState<string | undefined>(undefined);

  const { data: instanceData, isLoading: instancesLoading } = useWorkflowInstances(instanceStatus);
  const instances = instanceData?.instances ?? [];

  const myId = useAuthStore((s) => s.identity?.id);
  const allWorkflows = data?.workflows ?? [];
  const workflows = allWorkflows.filter((wf) => {
    if (ownerFilter === "mine") return !!myId && wf.owner === myId;
    if (ownerFilter === "global") return wf.visibility === "global";
    return true;
  });

  const handleDuplicate = useCallback(
    async (wf: WorkflowDef) => {
      try {
        const source = await workflowsApi.getSource(wf.id, wf.owner);
        const newId = `${wf.id}-copy`;
        const newToml = (source as string).replace(
          /id\s*=\s*"[^"]*"/,
          `id = "${newId}"`,
        );
        saveMutation.mutate(
          { id: newId, toml: newToml },
          { onSuccess: () => { refetch(); navigate(`/workflows/${newId}`); } },
        );
      } catch (err) {
        alert(`Duplicate failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    },
    [saveMutation, refetch, navigate],
  );

  const handleExportSingle = useCallback(async (wf: WorkflowDef) => {
    try {
      const source = await workflowsApi.getSource(wf.id, wf.owner);
      const blob = new Blob([source as string], { type: "application/toml" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${wf.id}.toml`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      alert(`Export failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }, []);

  const handleNewWorkflow = useCallback(() => {
    // Open an unsaved draft editor — nothing is written to disk until the user
    // sets an id and hits Save (see workflow-detail.tsx).
    navigate("/workflows/new");
  }, [navigate]);

  if (isLoading) return <PageSkeleton />;

  return (
    <div className="p-6 space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-2xl font-bold">Workflows</h1>
          <p className="text-muted-foreground">Manage workflow definitions, templates, and instances.</p>
        </div>
        <div className="flex items-center gap-2">
          <ImportExport onImported={() => refetch()} />
          <Button onClick={handleNewWorkflow} disabled={saveMutation.isPending}>
            <Plus className="h-4 w-4 mr-1.5" />
            New Workflow
          </Button>
        </div>
      </div>

      {/* Template gallery */}
      <TemplateGallery workflows={workflows} onUseTemplate={setTemplateWf} />

      {/* Tabs: Definitions / Instances */}
      <Tabs defaultValue="definitions">
        <div className="flex items-center justify-between gap-3 mb-4">
          <TabsList>
            <TabsTrigger value="definitions">Definitions</TabsTrigger>
            <TabsTrigger value="instances">Instances</TabsTrigger>
          </TabsList>
          {/* Owner filter + view toggle — only shown on definitions tab */}
          <TabsContent value="definitions" className="m-0 p-0 flex items-center gap-2">
            <div className="flex items-center border rounded-md">
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
            <div className="flex items-center border rounded-md">
              <Button
                variant={viewType === "grid" ? "secondary" : "ghost"}
                size="icon"
                className="h-8 w-8 rounded-r-none"
                onClick={() => setViewType("grid")}
              >
                <LayoutGrid className="h-3.5 w-3.5" />
              </Button>
              <Button
                variant={viewType === "list" ? "secondary" : "ghost"}
                size="icon"
                className="h-8 w-8 rounded-l-none"
                onClick={() => setViewType("list")}
              >
                <List className="h-3.5 w-3.5" />
              </Button>
            </div>
          </TabsContent>
        </div>

        <TabsContent value="definitions">
          {workflows.length === 0 ? (
            <EmptyState
              icon={GitBranch}
              title="No workflows"
              description="Create a new workflow or import one from a TOML file."
              action={{ label: "New Workflow", onClick: handleNewWorkflow }}
            />
          ) : viewType === "grid" ? (
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {workflows.map((wf) => (
                <WorkflowCard
                  key={`${wf.owner ?? "sys"}/${wf.id}`}
                  workflow={wf}
                  onDuplicate={handleDuplicate}
                  onExport={handleExportSingle}
                  onShare={setShareWf}
                  onTransfer={setTransferWf}
                  onDelete={setDeleteWf}
                />
              ))}
            </div>
          ) : (
            <WorkflowList
              workflows={workflows}
              onDuplicate={handleDuplicate}
              onExport={handleExportSingle}
              onShare={setShareWf}
              onTransfer={setTransferWf}
              onDelete={setDeleteWf}
            />
          )}
        </TabsContent>

        <TabsContent value="instances">
          {instancesLoading ? (
            <div className="flex items-center justify-center py-12 text-sm text-muted-foreground">
              Loading instances…
            </div>
          ) : (
            <InstanceTable
              instances={instances}
              onStatusFilter={setInstanceStatus}
              activeStatus={instanceStatus}
              showWorkflowId
            />
          )}
        </TabsContent>
      </Tabs>

      {/* Dialogs */}
      <QuickStartWizard
        workflow={templateWf}
        open={templateWf !== null}
        onOpenChange={(open) => { if (!open) setTemplateWf(null); }}
      />
      <DeleteWorkflowDialog
        workflow={deleteWf}
        open={deleteWf !== null}
        onOpenChange={(open) => { if (!open) setDeleteWf(null); }}
        onDeleted={() => refetch()}
      />
      {shareWf && (
        <ShareDialog
          kind="workflows"
          id={shareWf.id}
          owner={shareWf.owner}
          name={shareWf.name}
          open={shareWf !== null}
          onOpenChange={(open) => { if (!open) setShareWf(null); }}
        />
      )}
      {transferWf && (
        <TransferDialog
          kind="workflows"
          id={transferWf.id}
          owner={transferWf.owner}
          name={transferWf.name}
          open={transferWf !== null}
          onOpenChange={(open) => { if (!open) setTransferWf(null); }}
        />
      )}
    </div>
  );
}
