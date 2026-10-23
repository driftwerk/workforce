import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { useWorkflowInstancesForDef, useDeleteWorkflow } from "@/hooks/use-workflows";
import type { WorkflowDef } from "@/lib/api/workflows";

interface DeleteWorkflowDialogProps {
  workflow: WorkflowDef | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onDeleted?: () => void;
}

export function DeleteWorkflowDialog({ workflow, open, onOpenChange, onDeleted }: DeleteWorkflowDialogProps) {
  const { data: instanceData } = useWorkflowInstancesForDef(workflow?.id);
  const deleteMutation = useDeleteWorkflow();

  const runningCount = instanceData?.instances.filter(
    (i) => i.status === "running" || i.status === "paused",
  ).length ?? 0;

  const handleDelete = () => {
    if (!workflow) return;
    deleteMutation.mutate(
      { id: workflow.id, owner: workflow.owner },
      {
        onSuccess: () => {
          onOpenChange(false);
          onDeleted?.();
        },
      },
    );
  };

  if (!workflow) return null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete Workflow</DialogTitle>
          <DialogDescription>
            Are you sure you want to delete <strong>{workflow.name}</strong>? This action cannot be undone.
          </DialogDescription>
        </DialogHeader>
        {runningCount > 0 && (
          <div className="flex items-center gap-2 px-3 py-2 rounded-md bg-warning/10 border border-warning/20">
            <Badge variant="warning">{runningCount}</Badge>
            <span className="text-sm text-warning">active instance{runningCount > 1 ? "s" : ""} will be affected</span>
          </div>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button variant="destructive" onClick={handleDelete} disabled={deleteMutation.isPending}>
            {deleteMutation.isPending ? "Deleting…" : "Delete"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
