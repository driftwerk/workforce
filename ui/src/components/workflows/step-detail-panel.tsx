import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { formatCost, formatDateTime, formatDuration } from "@/lib/utils";
import type { TimelineStep } from "@/lib/api/workflows";
import { ChevronDown, ChevronRight } from "lucide-react";

type StatusVariant = "default" | "secondary" | "destructive" | "outline" | "success" | "warning";

const STATUS_VARIANT: Record<string, StatusVariant> = {
  completed: "success",
  failed: "destructive",
  paused: "warning",
  skipped: "secondary",
  running: "default",
  pending: "outline",
};

/** Longer values start folded so a review summary does not swamp the panel. */
const FOLD_ABOVE = 600;

interface StepDetailPanelProps {
  step: TimelineStep;
}

function OutputValue({ value }: { value: unknown }) {
  const [open, setOpen] = useState(false);
  const text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  const long = text.length > FOLD_ABOVE;
  const shown = long && !open ? `${text.slice(0, FOLD_ABOVE)}…` : text;
  return (
    <div className="space-y-1">
      <div className="break-all whitespace-pre-wrap">{shown}</div>
      {long && (
        <Button variant="ghost" size="sm" className="h-5 px-1 text-[10px] text-muted-foreground" onClick={() => setOpen(!open)}>
          {open ? "Show less" : `Show all (${text.length.toLocaleString()} chars)`}
        </Button>
      )}
    </div>
  );
}

function Meta({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <span className="text-muted-foreground">{label}</span>
      <div className="mt-0.5">{children}</div>
    </div>
  );
}

export function StepDetailPanel({ step }: StepDetailPanelProps) {
  const [showRaw, setShowRaw] = useState(false);
  const variant = STATUS_VARIANT[step.status] ?? "outline";
  const record = step.record;

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <div className="flex items-center gap-2">
          <h3 className="text-sm font-semibold">{step.name}</h3>
          <Badge variant={variant} className="text-[10px]">
            {step.status}
          </Badge>
          {record?.outcome && <span className="text-xs font-mono text-muted-foreground">→ {record.outcome}</span>}
        </div>
        <div className="text-xs text-muted-foreground font-mono">{step.step_id}</div>
      </div>

      {!record && (
        <div className="text-xs text-muted-foreground">No record yet: the instance is on this step now.</div>
      )}

      {record && (
        <div className="grid grid-cols-2 gap-2 text-xs">
          <Meta label="Started">{formatDateTime(record.started_at)}</Meta>
          <Meta label="Completed">{formatDateTime(record.completed_at)}</Meta>
          <Meta label="Duration">{formatDuration(record.duration_ms)}</Meta>
          <Meta label="Rounds">{record.rounds}</Meta>
          {record.engine && (
            <Meta label="Engine">
              <span className="font-mono">{record.engine}</span>
            </Meta>
          )}
          {record.model && (
            <Meta label="Model">
              <span className="font-mono">{record.model}</span>
            </Meta>
          )}
          {record.decided_first && (
            <Meta label={record.decided_first.settled ? "Settled first (no turn ran)" : "Decided first"}>
              <span className="font-mono">{record.decided_first.answer}</span>
            </Meta>
          )}
          <Meta label="Cost">{formatCost(record.cost_usd)}</Meta>
          {(record.input_tokens != null || record.output_tokens != null) && (
            <Meta label="Tokens">
              {(record.input_tokens ?? 0).toLocaleString()} in / {(record.output_tokens ?? 0).toLocaleString()} out
            </Meta>
          )}
          {record.budget_capped && <Meta label="Budget">Round budget reached</Meta>}
        </div>
      )}

      {record?.error && (
        <div className="space-y-1">
          <span className="text-xs text-destructive font-medium">Error</span>
          <div className="text-xs bg-destructive/10 text-destructive rounded-md p-2 font-mono whitespace-pre-wrap break-all">
            {record.error}
          </div>
        </div>
      )}

      {record?.report && (
        <div className="space-y-1">
          <span className="text-xs font-medium text-muted-foreground">Report</span>
          <div className="text-xs bg-muted/50 rounded-md p-2 whitespace-pre-wrap break-words">{record.report}</div>
        </div>
      )}

      {record?.outputs && Object.keys(record.outputs).length > 0 && (
        <div className="space-y-1.5">
          <span className="text-xs font-medium text-muted-foreground">Outputs</span>
          <div className="border rounded-md overflow-hidden">
            <table className="w-full text-xs">
              <tbody>
                {Object.entries(record.outputs).map(([key, val]) => (
                  <tr key={key} className="border-b last:border-b-0 align-top">
                    <td className="px-2 py-1.5 font-mono text-muted-foreground bg-muted/30 w-1/3">{key}</td>
                    <td className="px-2 py-1.5">
                      <OutputValue value={val} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {record?.raw_response && (
        <div className="space-y-1.5">
          <Button
            variant="ghost"
            size="sm"
            className="h-6 px-1 text-xs text-muted-foreground"
            onClick={() => setShowRaw(!showRaw)}
          >
            {showRaw ? <ChevronDown className="h-3 w-3 mr-1" /> : <ChevronRight className="h-3 w-3 mr-1" />}
            Raw Response
          </Button>
          {showRaw && (
            <div className="text-xs bg-muted/50 rounded-md p-2 font-mono whitespace-pre-wrap break-all max-h-60 overflow-y-auto">
              {record.raw_response}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
