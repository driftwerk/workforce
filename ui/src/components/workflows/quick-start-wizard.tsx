import { useState, useCallback } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { useRunWorkflow, useWorkflowSource } from "@/hooks/use-workflows";
import { parseWorkflowToml } from "@/lib/workflows/toml-parser";
import type { WorkflowDef } from "@/lib/api/workflows";

interface QuickStartWizardProps {
  workflow: WorkflowDef | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function QuickStartWizard({ workflow, open, onOpenChange }: QuickStartWizardProps) {
  const [step, setStep] = useState<"confirm" | "context">("confirm");
  const [contextValues, setContextValues] = useState<Record<string, string>>({});
  const [instanceId, setInstanceId] = useState<string | null>(null);

  const { data: source } = useWorkflowSource(workflow?.id, workflow?.owner);
  const runMutation = useRunWorkflow();

  const parsed = source ? parseWorkflowToml(source as string) : null;
  const contextFields = parsed?.def?.workflow.context ?? {};
  const hasContext = Object.keys(contextFields).length > 0;

  const handleReset = useCallback(() => {
    setStep("confirm");
    setContextValues({});
    setInstanceId(null);
  }, []);

  const handleClose = useCallback(
    (open: boolean) => {
      if (!open) handleReset();
      onOpenChange(open);
    },
    [onOpenChange, handleReset],
  );

  const handleNext = useCallback(() => {
    if (step === "confirm" && hasContext) {
      setStep("context");
    } else {
      handleRun();
    }
  }, [step, hasContext]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleRun = useCallback(() => {
    if (!workflow) return;
    const payload = Object.keys(contextValues).length > 0 ? contextValues : undefined;
    runMutation.mutate(
      { id: workflow.id, payload, owner: workflow.owner },
      {
        onSuccess: (data) => {
          setInstanceId(data.instance_id);
        },
      },
    );
  }, [workflow, contextValues, runMutation]);

  if (!workflow) return null;

  const requiredFields = Object.entries(contextFields).filter(([, f]) => f.required);
  const allRequiredFilled = requiredFields.every(([key]) => contextValues[key]?.trim());

  return (
    <Dialog open={open} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-lg">
        {instanceId ? (
          <>
            <DialogHeader>
              <DialogTitle>Workflow Started!</DialogTitle>
              <DialogDescription>
                Instance <code className="text-xs bg-muted px-1.5 py-0.5 rounded">{instanceId}</code> is now running.
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <Button onClick={() => handleClose(false)}>Done</Button>
            </DialogFooter>
          </>
        ) : step === "confirm" ? (
          <>
            <DialogHeader>
              <DialogTitle>Start: {workflow.name}</DialogTitle>
              <DialogDescription>{workflow.description || "No description"}</DialogDescription>
            </DialogHeader>
            <div className="space-y-2 py-2">
              <div className="flex items-center gap-2">
                <Badge variant="secondary">{workflow.steps} steps</Badge>
                {hasContext && <Badge variant="outline">{Object.keys(contextFields).length} context fields</Badge>}
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => handleClose(false)}>Cancel</Button>
              <Button onClick={handleNext} disabled={runMutation.isPending}>
                {hasContext ? "Next" : "Start"}
              </Button>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>Context Variables</DialogTitle>
              <DialogDescription>Fill in the required context for this workflow.</DialogDescription>
            </DialogHeader>
            <div className="space-y-3 py-2 max-h-[50vh] overflow-y-auto">
              {Object.entries(contextFields).map(([key, field]) => (
                <div key={key} className="space-y-1">
                  <label className="text-sm font-medium flex items-center gap-1">
                    {key}
                    {field.required && <span className="text-destructive">*</span>}
                  </label>
                  {field.description && <p className="text-xs text-muted-foreground">{field.description}</p>}
                  <Input
                    value={contextValues[key] ?? ""}
                    onChange={(e) => setContextValues((prev) => ({ ...prev, [key]: e.target.value }))}
                    placeholder={field.description || key}
                  />
                </div>
              ))}
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setStep("confirm")}>Back</Button>
              <Button onClick={handleRun} disabled={!allRequiredFilled || runMutation.isPending}>
                {runMutation.isPending ? "Starting…" : "Start Workflow"}
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
