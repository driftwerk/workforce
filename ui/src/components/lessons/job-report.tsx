import type { JobStatus } from "@/lib/api/lessons";

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-md border border-border bg-muted/20 px-3 py-2">
      <div className="text-lg font-semibold tabular-nums">{value.toLocaleString()}</div>
      <div className="text-xs text-muted-foreground">{label}</div>
    </div>
  );
}

/** Render a finished job's report (cleanup or backfill), the running state, or
 *  a failure. The report shape is discriminated by `kind`. */
export function JobReport({ job }: { job: JobStatus | undefined }) {
  if (!job) return null;

  if (job.status === "running") {
    return (
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <span className="h-3 w-3 animate-spin rounded-full border-2 border-current border-t-transparent" />
        Running…
      </div>
    );
  }

  if (job.status === "failed") {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
        Job failed: {job.error}
      </div>
    );
  }

  return (
    <div className="space-y-2">
      {job.dry_run && (
        <p className="text-xs text-amber-600 dark:text-amber-400">
          Dry run — this was a preview; nothing was changed.
        </p>
      )}
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
        {job.kind === "cleanup" ? (
          <>
            <Stat label="Scanned" value={job.report.lessons_scanned} />
            <Stat label="Pruned" value={job.report.pruned} />
            <Stat label="Merged" value={job.report.merged} />
            <Stat label="Kept" value={job.report.kept} />
            <Stat label="Errors" value={job.report.errors} />
          </>
        ) : (
          <>
            <Stat label="Agents scanned" value={job.report.agents_scanned} />
            <Stat label="Sessions scanned" value={job.report.sessions_scanned} />
            <Stat label="Turns scanned" value={job.report.turns_scanned} />
            <Stat label="Signals detected" value={job.report.signals_detected} />
            <Stat label="Lessons written" value={job.report.lessons_written} />
          </>
        )}
      </div>
    </div>
  );
}
