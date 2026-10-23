import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  useAbortWorkflow,
  useResumeWorkflow,
  useRejectWorkflow,
  useRetryStep,
  useSkipStep,
  useGotoStep,
} from "@/hooks/use-workflows";
import { formatCost, formatDuration } from "@/lib/utils";
import { apiErrorMessage } from "@/lib/api-error";
import type { WorkflowInstance, InstanceCost, InstanceStep } from "@/lib/api/workflows";
import {
  XCircle,
  Play,
  ThumbsDown,
  RotateCcw,
  SkipForward,
  ArrowRightCircle,
} from "lucide-react";

interface InstanceActionsToolbarProps {
  instance: WorkflowInstance;
  cost: InstanceCost | null | undefined;
  steps: InstanceStep[];
  currentStepId: string | null;
}

type ModalType = "abort" | "reject" | "retry" | "skip" | "goto" | null;

export function InstanceActionsToolbar({
  instance,
  cost,
  steps,
  currentStepId,
}: InstanceActionsToolbarProps) {
  const [modal, setModal] = useState<ModalType>(null);
  const [feedback, setFeedback] = useState("");
  const [targetStepId, setTargetStepId] = useState("");

  const abortMutation = useAbortWorkflow();
  const resumeMutation = useResumeWorkflow();
  const rejectMutation = useRejectWorkflow();
  const retryMutation = useRetryStep();
  const skipMutation = useSkipStep();
  const gotoMutation = useGotoStep();

  const awaitingDecision = !!instance.decision;
  const isActive =
    instance.status === "running" || instance.status === "paused";
  const canResume = instance.status === "paused" && !awaitingDecision;
  const isSteerable = isActive && !awaitingDecision;

  const handleConfirm = () => {
    switch (modal) {
      case "abort":
        abortMutation.mutate(instance.instance_id, {
          onSuccess: () => setModal(null),
        });
        break;
      case "reject":
        rejectMutation.mutate(
          { id: instance.instance_id, reason: "rejected", feedback: feedback || undefined },
          { onSuccess: () => { setModal(null); setFeedback(""); } },
        );
        break;
      case "retry":
        if (currentStepId) {
          retryMutation.mutate(
            { instanceId: instance.instance_id, stepId: currentStepId },
            { onSuccess: () => setModal(null) },
          );
        }
        break;
      case "skip":
        if (currentStepId) {
          skipMutation.mutate(
            { instanceId: instance.instance_id, stepId: currentStepId },
            { onSuccess: () => setModal(null) },
          );
        }
        break;
      case "goto":
        if (targetStepId) {
          gotoMutation.mutate(
            { instanceId: instance.instance_id, stepId: targetStepId },
            { onSuccess: () => { setModal(null); setTargetStepId(""); } },
          );
        }
        break;
    }
  };

  const isPending =
    abortMutation.isPending ||
    resumeMutation.isPending ||
    rejectMutation.isPending ||
    retryMutation.isPending ||
    skipMutation.isPending ||
    gotoMutation.isPending;

  // Build unique step IDs from steps data for goto dropdown
  const stepIds = [...new Set(steps.map((s) => s.step_id))];

  return (
    <>
      <div className="flex items-center gap-1.5 flex-wrap">
        {canResume && (
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            disabled={isPending}
            onClick={() => resumeMutation.mutate({ id: instance.instance_id })}
          >
            <Play className="h-3 w-3 mr-1" />
            Resume
          </Button>
        )}
        {canResume && (
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            disabled={isPending}
            onClick={() => setModal("reject")}
          >
            <ThumbsDown className="h-3 w-3 mr-1" />
            Reject
          </Button>
        )}
        {isSteerable && currentStepId && (
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            disabled={isPending}
            onClick={() => setModal("retry")}
          >
            <RotateCcw className="h-3 w-3 mr-1" />
            Retry Step
          </Button>
        )}
        {isSteerable && currentStepId && (
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            disabled={isPending}
            onClick={() => setModal("skip")}
          >
            <SkipForward className="h-3 w-3 mr-1" />
            Skip Step
          </Button>
        )}
        {isSteerable && stepIds.length > 1 && (
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            disabled={isPending}
            onClick={() => setModal("goto")}
          >
            <ArrowRightCircle className="h-3 w-3 mr-1" />
            Goto Step
          </Button>
        )}
        {isActive && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 text-xs text-destructive hover:text-destructive"
            disabled={isPending}
            onClick={() => {
              abortMutation.reset();
              setModal("abort");
            }}
          >
            <XCircle className="h-3 w-3 mr-1" />
            Abort
          </Button>
        )}
      </div>

      {/* Abort confirmation */}
      <Dialog open={modal === "abort"} onOpenChange={(open) => !open && setModal(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Abort Workflow Instance</DialogTitle>
            <DialogDescription>
              Are you sure you want to abort this workflow instance? This action cannot be undone.
            </DialogDescription>
          </DialogHeader>
          {cost && (
            <div className="text-sm space-y-1 bg-muted/50 rounded-md p-3">
              <div>Cost so far: <span className="font-semibold">{formatCost(cost.total_cost)}</span></div>
              <div>Duration: <span className="font-semibold">{formatDuration(cost.total_duration_ms)}</span></div>
              <div>Rounds: <span className="font-semibold">{cost.total_rounds}</span></div>
            </div>
          )}
          {abortMutation.error && (
            <p className="text-sm text-destructive">{apiErrorMessage(abortMutation.error)}</p>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setModal(null)}>Cancel</Button>
            <Button variant="destructive" disabled={abortMutation.isPending} onClick={handleConfirm}>
              {abortMutation.isPending ? "Aborting..." : "Abort"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Reject confirmation */}
      <Dialog open={modal === "reject"} onOpenChange={(open) => !open && setModal(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Reject Workflow</DialogTitle>
            <DialogDescription>Provide optional feedback for why this workflow is being rejected.</DialogDescription>
          </DialogHeader>
          <Input
            value={feedback}
            onChange={(e) => setFeedback(e.target.value)}
            placeholder="Rejection reason..."
          />
          <DialogFooter>
            <Button variant="outline" onClick={() => setModal(null)}>Cancel</Button>
            <Button variant="destructive" disabled={rejectMutation.isPending} onClick={handleConfirm}>
              {rejectMutation.isPending ? "Rejecting..." : "Reject"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Retry confirmation */}
      <Dialog open={modal === "retry"} onOpenChange={(open) => !open && setModal(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Retry Step</DialogTitle>
            <DialogDescription>
              Retry the current step: <span className="font-mono font-semibold">{currentStepId}</span>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setModal(null)}>Cancel</Button>
            <Button disabled={retryMutation.isPending} onClick={handleConfirm}>
              {retryMutation.isPending ? "Retrying..." : "Retry"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Skip confirmation */}
      <Dialog open={modal === "skip"} onOpenChange={(open) => !open && setModal(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Skip Step</DialogTitle>
            <DialogDescription>
              Skip the current step: <span className="font-mono font-semibold">{currentStepId}</span>
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setModal(null)}>Cancel</Button>
            <Button disabled={skipMutation.isPending} onClick={handleConfirm}>
              {skipMutation.isPending ? "Skipping..." : "Skip"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Goto step dialog */}
      <Dialog open={modal === "goto"} onOpenChange={(open) => !open && setModal(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Go to Step</DialogTitle>
            <DialogDescription>Select a step to jump to.</DialogDescription>
          </DialogHeader>
          <div className="space-y-2">
            {stepIds.map((sid) => (
              <Button
                key={sid}
                variant={targetStepId === sid ? "secondary" : "outline"}
                size="sm"
                className="mr-2 text-xs font-mono"
                onClick={() => setTargetStepId(sid)}
              >
                {sid}
              </Button>
            ))}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setModal(null)}>Cancel</Button>
            <Button disabled={gotoMutation.isPending || !targetStepId} onClick={handleConfirm}>
              {gotoMutation.isPending ? "Going..." : "Go to Step"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
