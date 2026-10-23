import { useState } from "react";
import { useNavigate } from "react-router";
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
import { useCreateAgent } from "@/hooks/use-agents";

interface CreateAgentDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Pre-set owner for team-scoped agent creation. */
  ownerId?: string;
  ownerType?: string;
}

export function CreateAgentDialog({ open, onOpenChange, ownerId, ownerType }: CreateAgentDialogProps) {
  const navigate = useNavigate();
  const createMutation = useCreateAgent();

  const [agentId, setAgentId] = useState("");
  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [isDefault, setIsDefault] = useState(false);
  const [recallScope, setRecallScope] = useState<"shared" | "isolated">("shared");

  function reset() {
    setAgentId("");
    setName("");
    setModel("");
    setIsDefault(false);
    setRecallScope("shared");
  }

  function handleSubmit() {
    if (!agentId.trim()) return;
    createMutation.mutate(
      {
        id: agentId.trim(),
        name: name.trim() || undefined,
        model: model.trim() || undefined,
        default: isDefault || undefined,
        owner_id: ownerId,
        owner_type: ownerType,
        agent_recall_scope: recallScope === "isolated" ? "isolated" : undefined,
      },
      {
        onSuccess: () => {
          onOpenChange(false);
          reset();
          navigate(`/agents/${agentId.trim()}`);
        },
      },
    );
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create Agent</DialogTitle>
          <DialogDescription>
            Create a new AI agent with its own identity and configuration.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-1.5">
            <label htmlFor="agent-id" className="text-sm font-medium leading-none">
              Agent ID <span className="text-destructive">*</span>
            </label>
            <Input
              id="agent-id"
              placeholder="e.g. code-reviewer"
              value={agentId}
              onChange={(e) => setAgentId(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <label htmlFor="agent-name" className="text-sm font-medium leading-none">
              Display Name
            </label>
            <Input
              id="agent-name"
              placeholder="e.g. Code Reviewer"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <label htmlFor="agent-model" className="text-sm font-medium leading-none">
              Model
            </label>
            <Input
              id="agent-model"
              placeholder="e.g. claude-sonnet-5"
              value={model}
              onChange={(e) => setModel(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium leading-none">Memory</label>
            <label className="flex items-start gap-2 text-sm">
              <input
                type="radio"
                name="recall-scope"
                checked={recallScope === "shared"}
                onChange={() => setRecallScope("shared")}
                className="mt-0.5"
              />
              <span>
                <span className="font-medium">Shared</span>
                <span className="block text-xs text-muted-foreground">
                  Recalls your memories from conversations with your other
                  agents.
                </span>
              </span>
            </label>
            <label className="flex items-start gap-2 text-sm">
              <input
                type="radio"
                name="recall-scope"
                checked={recallScope === "isolated"}
                onChange={() => setRecallScope("isolated")}
                className="mt-0.5"
              />
              <span>
                <span className="font-medium">Fresh</span>
                <span className="block text-xs text-muted-foreground">
                  Starts empty — recalls only what this agent learns itself.
                </span>
              </span>
            </label>
          </div>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={isDefault}
              onChange={(e) => setIsDefault(e.target.checked)}
              className="rounded border-input"
            />
            Set as default agent
          </label>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            disabled={!agentId.trim() || createMutation.isPending}
            onClick={handleSubmit}
          >
            {createMutation.isPending ? "Creating..." : "Create"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
