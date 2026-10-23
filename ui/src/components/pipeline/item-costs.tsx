import { Fragment } from "react";
import { ChevronRight } from "lucide-react";
import { formatCostScaled, formatDateTime, formatTokens } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Skeleton } from "@/components/ui/skeleton";
import { InstanceStatusBadge } from "@/components/workflows/instance-status-badge";
import { usePipelineItemCosts } from "@/hooks/use-pipeline";
import type { Billing, RunCosts, StageCosts, StepCosts } from "@/lib/api/pipelines";
import {
  costsErrorText,
  coverageNotes,
  engineAndModel,
  modelChip,
  nothingMatched,
  stillRunning,
} from "@/lib/pipeline/costs";
import type { Item } from "./board";
import { CoverageNotes } from "./coverage-notes";
import { Stat } from "./stat";

export function ItemCostsPanel({
  scope,
  id,
  item,
  itemLabel,
}: {
  scope: string;
  id: string;
  item: Item;
  itemLabel: string;
}) {
  const { data, error } = usePipelineItemCosts(scope, id, item.id, item.body.updated_at);
  const costs = data?.item === item.id ? data : undefined;

  if (!costs) {
    return error ? (
      <p className="text-sm text-muted-foreground">{costsErrorText(error)}</p>
    ) : (
      <Skeleton className="h-40 w-full" />
    );
  }

  const notes = coverageNotes(costs, itemLabel);
  return (
    <section aria-label="cost" className="space-y-4">
      <div className="flex items-center gap-2">
        <h3 className="text-sm font-semibold">Cost</h3>
        {stillRunning(costs) && <Badge variant="info">so far</Badge>}
      </div>
      {!nothingMatched(costs.coverage) && (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <Stat label="Billed" value={formatCostScaled(costs.total.billed_usd)} />
          <Stat label="Estimated (subscriptions)" value={formatCostScaled(costs.total.estimated_usd)} tone="muted" />
          <Stat
            label="Tokens"
            value={`${formatTokens(costs.total.input_tokens)} in / ${formatTokens(costs.total.output_tokens)} out`}
          />
          <Stat label="Runs" value={costs.total.runs} />
        </div>
      )}
      <CoverageNotes notes={notes} />
      {costs.stages.length > 0 && <Stages stages={costs.stages} />}
      {costs.runs.length > 0 && (
        <div className="space-y-2">
          {costs.runs.map((run) => (
            <Run key={run.run} run={run} counted={run.booked_to === costs.item} />
          ))}
        </div>
      )}
      {costs.coverage.runs_named === 0 && (
        <p className="text-xs text-muted-foreground">No workflow run has recorded against this {itemLabel} yet.</p>
      )}
    </section>
  );
}

function Cost({ usd, billing }: { usd: number; billing: Billing | null }) {
  return billing === "estimated" ? (
    <span className="text-muted-foreground">{formatCostScaled(usd)} (estimate)</span>
  ) : (
    <span>{formatCostScaled(usd)}</span>
  );
}

function Stages({ stages }: { stages: StageCosts[] }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs">
        <thead>
          <tr className="border-b text-left text-muted-foreground">
            <th className="py-1.5 pr-3 font-medium">Stage</th>
            <th className="py-1.5 pr-3 text-right font-medium">Runs</th>
            <th className="py-1.5 pr-3 font-medium">Models</th>
            <th className="py-1.5 pr-3 text-right font-medium">Billed</th>
            <th className="py-1.5 text-right font-medium">Estimated</th>
          </tr>
        </thead>
        <tbody>
          {stages.map((stage) => (
            <tr key={stage.stage} className="border-b align-top last:border-0">
              <td className="py-1.5 pr-3 font-medium">{stage.label}</td>
              <td className="py-1.5 pr-3 text-right tabular-nums">{stage.runs}</td>
              <td className="py-1.5 pr-3">
                <div className="flex flex-wrap gap-1">
                  {stage.models.map((m) => (
                    <Badge key={`${m.engine}|${m.model}|${m.billing}`} variant="outline" className="gap-1 font-normal">
                      {modelChip(m)}
                      <Cost usd={m.cost_usd} billing={m.billing} />
                    </Badge>
                  ))}
                </div>
              </td>
              <td className="py-1.5 pr-3 text-right tabular-nums">{formatCostScaled(stage.billed_usd)}</td>
              <td className="py-1.5 text-right tabular-nums text-muted-foreground">
                {formatCostScaled(stage.estimated_usd)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function RunSummary({ run }: { run: RunCosts }) {
  return (
    <>
      <span className="font-medium text-foreground">{run.workflow}</span>
      <InstanceStatusBadge status={run.status} />
      <span>{run.label}</span>
      <span className="font-mono" title={run.run}>
        {run.run.slice(0, 8)}
      </span>
      <span>{formatDateTime(run.started_at)}</span>
    </>
  );
}

function Run({ run, counted }: { run: RunCosts; counted: boolean }) {
  if (!counted) {
    return (
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border px-3 py-2 text-xs text-muted-foreground">
        <RunSummary run={run} />
        <span className="ml-auto">Counted on {run.booked_to}</span>
      </div>
    );
  }
  return (
    <Collapsible className="rounded-md border">
      <CollapsibleTrigger className="group flex w-full flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-left">
        <ChevronRight className="h-3.5 w-3.5 shrink-0 transition-transform group-data-[state=open]:rotate-90" />
        <RunSummary run={run} />
        <span className="ml-auto tabular-nums text-foreground">
          {formatCostScaled(run.billed_usd)}
          {run.estimated_usd > 0 && (
            <span className="text-muted-foreground"> · {formatCostScaled(run.estimated_usd)} estimated</span>
          )}
        </span>
      </CollapsibleTrigger>
      <CollapsibleContent className="border-t px-3 py-2">
        <Steps steps={run.steps} />
      </CollapsibleContent>
    </Collapsible>
  );
}

function Steps({ steps }: { steps: StepCosts[] }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs">
        <thead>
          <tr className="border-b text-left text-muted-foreground">
            <th className="py-1 pr-3 font-medium">Step</th>
            <th className="py-1 pr-3 font-medium">Engine · model</th>
            <th className="py-1 pr-3 text-right font-medium">Rounds</th>
            <th className="py-1 pr-3 text-right font-medium">Tokens</th>
            <th className="py-1 pr-3 text-right font-medium">Cost</th>
            <th className="py-1 font-medium">Status</th>
          </tr>
        </thead>
        <tbody>
          {steps.map((step, i) => (
            <Fragment key={`${step.step_id}-${i}`}>
              <tr className="align-top">
                <td className="py-1 pr-3 font-mono">{step.step_id}</td>
                <td className="py-1 pr-3">{engineAndModel(step)}</td>
                <td className="py-1 pr-3 text-right tabular-nums">{step.rounds}</td>
                <td className="py-1 pr-3 text-right tabular-nums">
                  {step.input_tokens === null
                    ? "—"
                    : `${formatTokens(step.input_tokens)} / ${formatTokens(step.output_tokens ?? 0)}`}
                </td>
                <td className="py-1 pr-3 text-right tabular-nums">
                  {step.cost_usd === null ? (
                    <span className="text-muted-foreground">not recorded</span>
                  ) : (
                    <Cost usd={step.cost_usd} billing={step.billing} />
                  )}
                </td>
                <td className="py-1">
                  {step.failed ? (
                    <Badge variant="destructive">Failed</Badge>
                  ) : step.budget_capped ? (
                    <Badge variant="warning">Budget capped</Badge>
                  ) : (
                    <span className="text-muted-foreground">Completed</span>
                  )}
                </td>
              </tr>
              {step.handed_over.map((attempt, j) => (
                <tr key={`${step.step_id}-${i}-handed-${j}`} className="text-muted-foreground">
                  <td />
                  <td colSpan={5} className="py-0.5 pl-4">
                    ↳ handed over from {modelChip(attempt)} <Cost usd={attempt.cost_usd} billing={attempt.billing} />
                    {attempt.error && (
                      <span className="ml-1 inline-block max-w-md truncate align-bottom" title={attempt.error}>
                        · {attempt.error}
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </Fragment>
          ))}
        </tbody>
      </table>
    </div>
  );
}
