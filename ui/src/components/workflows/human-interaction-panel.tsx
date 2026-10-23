import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { useResumeWorkflow, useRejectWorkflow } from "@/hooks/use-workflows";
import type { Branch, DecisionChoice, DecisionView, TimelineStep, WorkflowInstance } from "@/lib/api/workflows";
import { choiceVariant, expiryText, waitingOnSentence } from "@/lib/workflows/decision";
import { apiErrorMessage } from "@/lib/api-error";
import { MessageSquare, Play, XCircle, ArrowRight } from "lucide-react";

interface HumanInteractionPanelProps {
  instance: WorkflowInstance;
  currentStep: TimelineStep | null;
  branches: Branch[];
}

export function HumanInteractionPanel({ instance, currentStep, branches }: HumanInteractionPanelProps) {
  const [feedback, setFeedback] = useState("");
  const resumeMutation = useResumeWorkflow();
  const rejectMutation = useRejectWorkflow();

  if (instance.status !== "paused") return null;

  const handleResume = (outcome?: string) => {
    resumeMutation.mutate({ id: instance.instance_id, outcome });
  };

  const handleReject = () => {
    rejectMutation.mutate({
      id: instance.instance_id,
      reason: "rejected",
      feedback: feedback || undefined,
    });
    setFeedback("");
  };

  const isPending = resumeMutation.isPending || rejectMutation.isPending;
  const error = resumeMutation.error ?? rejectMutation.error;

  return (
    <Card className="border-warning/40 bg-warning/5">
      <CardHeader className="p-4 pb-2">
        <CardTitle className="text-sm flex items-center gap-2">
          <MessageSquare className="h-4 w-4 text-warning" />
          Awaiting Human Input
          <Badge variant="warning" className="text-[10px]">Paused</Badge>
        </CardTitle>
      </CardHeader>
      <CardContent className="p-4 pt-2 space-y-3">
        {instance.decision ? (
          <DecisionPanel instanceId={instance.instance_id} decision={instance.decision}>
            <StepOutputs step={currentStep} />
          </DecisionPanel>
        ) : (
          <>
            {/* What the workflow is waiting for */}
            {currentStep && (
              <div className="text-xs text-muted-foreground">
                Step <span className="font-mono font-medium text-foreground">{currentStep.name}</span> is
                waiting for a decision.
              </div>
            )}

            <StepOutputs step={currentStep} />

            {/* Branch option buttons */}
            {branches.length > 0 && (
              <div className="space-y-1.5">
                <span className="text-xs font-medium text-muted-foreground">Choose a path:</span>
                <div className="flex flex-wrap gap-2">
                  {branches.map((branch) => (
                    <Button
                      key={branch.when}
                      variant="outline"
                      size="sm"
                      className="h-7 text-xs"
                      disabled={isPending}
                      onClick={() => handleResume(branch.when)}
                    >
                      <ArrowRight className="h-3 w-3 mr-1" />
                      {branch.when}
                    </Button>
                  ))}
                </div>
              </div>
            )}

            {/* Feedback input for rejection */}
            <div className="space-y-1.5">
              <span className="text-xs font-medium text-muted-foreground">Feedback (optional):</span>
              <Input
                value={feedback}
                onChange={(e) => setFeedback(e.target.value)}
                placeholder="Add feedback or reason..."
                className="h-8 text-xs"
                disabled={isPending}
              />
            </div>

            {/* Action buttons */}
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                className="h-7 text-xs"
                disabled={isPending}
                onClick={() => handleResume()}
              >
                <Play className="h-3 w-3 mr-1" />
                {resumeMutation.isPending ? "Resuming..." : "Resume"}
              </Button>
              <Button
                variant="destructive"
                size="sm"
                className="h-7 text-xs"
                disabled={isPending}
                onClick={handleReject}
              >
                <XCircle className="h-3 w-3 mr-1" />
                {rejectMutation.isPending ? "Rejecting..." : "Reject"}
              </Button>
            </div>
            {error && <p className="text-xs text-destructive">{apiErrorMessage(error)}</p>}
          </>
        )}
      </CardContent>
    </Card>
  );
}

function StepOutputs({ step }: { step: TimelineStep | null }) {
  const outputs = step?.record?.outputs;
  if (!outputs || Object.keys(outputs).length === 0) return null;
  return (
    <div className="bg-card border rounded-md p-2.5 text-xs space-y-1">
      {Object.entries(outputs).map(([key, val]) => (
        <div key={key}>
          <span className="text-muted-foreground font-mono">{key}:</span>{" "}
          <span>{typeof val === "string" ? val : JSON.stringify(val)}</span>
        </div>
      ))}
    </div>
  );
}

interface DecisionPanelProps {
  instanceId: string;
  decision: DecisionView;
  children: ReactNode;
}

function DecisionPanel({ instanceId, decision, children }: DecisionPanelProps) {
  const resumeMutation = useResumeWorkflow();
  const deciders = waitingOnSentence(decision.waiting_on);
  const expiry = expiryText(decision.expires_at, decision.on_expiry);

  const decide = (choice: DecisionChoice) => {
    if (choice.confirm && !confirm(choice.confirm)) return;
    resumeMutation.mutate({ id: instanceId, outcome: choice.outcome, gate: decision.gate });
  };

  return (
    <div className="space-y-2">
      <div className="text-sm font-medium">{decision.title}</div>
      {decision.context && <div className="text-xs text-muted-foreground">{decision.context}</div>}
      {children}
      <div className="text-xs text-muted-foreground">
        Waiting on {deciders}
        {expiry && <> · {expiry}</>}
      </div>
      {decision.may_decide ? (
        <div className="flex flex-wrap gap-2">
          {decision.choices.map((choice) => (
            <Button
              key={choice.outcome}
              variant={choiceVariant(choice.style)}
              size="sm"
              className="h-7 text-xs"
              disabled={resumeMutation.isPending}
              onClick={() => decide(choice)}
            >
              {choice.label}
            </Button>
          ))}
        </div>
      ) : (
        <div className="text-xs text-muted-foreground">Only {deciders} can decide this.</div>
      )}
      {resumeMutation.error && <p className="text-xs text-destructive">{apiErrorMessage(resumeMutation.error)}</p>}
    </div>
  );
}
