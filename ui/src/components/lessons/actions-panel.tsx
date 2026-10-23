import { useState } from "react";
import { Brush, History } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
  useBackfillJob,
  useCleanupJob,
  useInvalidateCorpusOnJobDone,
  useJobStatus,
} from "@/hooks/use-lessons";
import type { AgentOption } from "./agent-select";
import { JobReport } from "./job-report";

interface PanelProps {
  /** Agent currently selected on the page; used as the dialog default. */
  agent: string;
  agents: AgentOption[];
}

const SELECT_CLASS =
  "h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring";

export function ActionsPanel({ agent, agents }: PanelProps) {
  const [open, setOpen] = useState<null | "cleanup" | "backfill">(null);

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-base">Actions</CardTitle>
        <p className="text-sm text-muted-foreground">
          Groom the corpus or backfill lessons from past sessions. Both default
          to a safe preview.
        </p>
      </CardHeader>
      <CardContent className="flex flex-wrap gap-3">
        <Button variant="outline" className="gap-2" onClick={() => setOpen("cleanup")}>
          <Brush className="h-4 w-4" />
          Run Cleanup
        </Button>
        <Button variant="outline" className="gap-2" onClick={() => setOpen("backfill")}>
          <History className="h-4 w-4" />
          Run Backfill
        </Button>
      </CardContent>

      {open === "cleanup" && (
        <CleanupDialog
          agent={agent}
          agents={agents}
          onClose={() => setOpen(null)}
        />
      )}
      {open === "backfill" && (
        <BackfillDialog
          agent={agent}
          agents={agents}
          onClose={() => setOpen(null)}
        />
      )}
    </Card>
  );
}

interface DialogProps {
  agent: string;
  agents: AgentOption[];
  onClose: () => void;
}

function AgentField({
  value,
  onChange,
  agents,
}: {
  value: string;
  onChange: (v: string) => void;
  agents: AgentOption[];
}) {
  return (
    <label className="block space-y-1">
      <span className="text-sm font-medium">Agent</span>
      <select
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={SELECT_CLASS}
      >
        {agents.map((a) => (
          <option key={a.value} value={a.value}>
            {a.label}
          </option>
        ))}
      </select>
    </label>
  );
}

function DryRunField({
  checked,
  onChange,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <label className="flex items-center gap-2 text-sm">
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="h-4 w-4 rounded border-input"
      />
      Dry run (preview only — nothing is changed)
    </label>
  );
}

function CleanupDialog({ agent, agents, onClose }: DialogProps) {
  const [targetAgent, setTargetAgent] = useState(agent);
  const [dryRun, setDryRun] = useState(true);
  const [mergeSimilarity, setMergeSimilarity] = useState("");
  // Two-step guard for the destructive (non-dry-run) apply.
  const [confirmApply, setConfirmApply] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);

  const kick = useCleanupJob();
  const { data: job } = useJobStatus(jobId);
  useInvalidateCorpusOnJobDone(job);
  const running = job?.status === "running" || kick.isPending;

  async function submit() {
    if (!dryRun && !confirmApply) {
      setConfirmApply(true);
      return;
    }
    const merge = mergeSimilarity.trim() ? Number(mergeSimilarity) : undefined;
    const res = await kick.mutateAsync({
      agent: targetAgent,
      dry_run: dryRun,
      merge_similarity: Number.isFinite(merge) ? merge : undefined,
    });
    setJobId(res.job_id);
    setConfirmApply(false);
  }

  const applyArmed = !dryRun && confirmApply;

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Run cleanup</DialogTitle>
          <DialogDescription>
            Prune low-value lessons and merge near-duplicates. A dry run only
            previews what would change.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <AgentField value={targetAgent} onChange={setTargetAgent} agents={agents} />
          <DryRunField
            checked={dryRun}
            onChange={(v) => {
              setDryRun(v);
              setConfirmApply(false);
            }}
            disabled={running}
          />
          <label className="block space-y-1">
            <span className="text-sm font-medium">
              Merge similarity{" "}
              <span className="font-normal text-muted-foreground">(optional, 0–1)</span>
            </span>
            <Input
              type="number"
              step="0.01"
              min="0"
              max="1"
              value={mergeSimilarity}
              onChange={(e) => setMergeSimilarity(e.target.value)}
              placeholder="default"
              disabled={running}
            />
          </label>

          {applyArmed && (
            <p className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
              This will prune and merge lessons for real — this is irreversible.
              Press <strong>Apply — irreversible</strong> again to proceed.
            </p>
          )}

          {kick.isError && (
            <p className="text-sm text-destructive">
              Failed to start:{" "}
              {kick.error instanceof Error ? kick.error.message : "Unknown error"}
            </p>
          )}

          {jobId && <JobReport job={job} />}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Close
          </Button>
          <Button
            variant={applyArmed ? "destructive" : "default"}
            onClick={submit}
            disabled={running}
          >
            {running
              ? "Running…"
              : dryRun
                ? "Preview cleanup"
                : applyArmed
                  ? "Apply — irreversible"
                  : "Apply cleanup…"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function BackfillDialog({ agent, agents, onClose }: DialogProps) {
  const [targetAgent, setTargetAgent] = useState(agent);
  const [dryRun, setDryRun] = useState(true);
  const [sinceDays, setSinceDays] = useState("");
  const [confirmApply, setConfirmApply] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);

  const kick = useBackfillJob();
  const { data: job } = useJobStatus(jobId);
  useInvalidateCorpusOnJobDone(job);
  const running = job?.status === "running" || kick.isPending;

  async function submit() {
    if (!dryRun && !confirmApply) {
      setConfirmApply(true);
      return;
    }
    const since = sinceDays.trim() ? Number(sinceDays) : undefined;
    const res = await kick.mutateAsync({
      agent: targetAgent,
      dry_run: dryRun,
      since_days: Number.isFinite(since) ? since : undefined,
    });
    setJobId(res.job_id);
    setConfirmApply(false);
  }

  const applyArmed = !dryRun && confirmApply;

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Run backfill</DialogTitle>
          <DialogDescription>
            Mine past sessions for lessons that were never extracted. A dry run
            only previews what would be written.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <AgentField value={targetAgent} onChange={setTargetAgent} agents={agents} />
          <DryRunField
            checked={dryRun}
            onChange={(v) => {
              setDryRun(v);
              setConfirmApply(false);
            }}
            disabled={running}
          />
          <label className="block space-y-1">
            <span className="text-sm font-medium">
              Since days{" "}
              <span className="font-normal text-muted-foreground">(optional)</span>
            </span>
            <Input
              type="number"
              step="1"
              min="1"
              value={sinceDays}
              onChange={(e) => setSinceDays(e.target.value)}
              placeholder="all history"
              disabled={running}
            />
          </label>

          {applyArmed && (
            <p className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
              This will write new lessons to the corpus for real. Press{" "}
              <strong>Apply — irreversible</strong> again to proceed.
            </p>
          )}

          {kick.isError && (
            <p className="text-sm text-destructive">
              Failed to start:{" "}
              {kick.error instanceof Error ? kick.error.message : "Unknown error"}
            </p>
          )}

          {jobId && <JobReport job={job} />}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Close
          </Button>
          <Button
            variant={applyArmed ? "destructive" : "default"}
            onClick={submit}
            disabled={running}
          >
            {running
              ? "Running…"
              : dryRun
                ? "Preview backfill"
                : applyArmed
                  ? "Apply — irreversible"
                  : "Apply backfill…"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
