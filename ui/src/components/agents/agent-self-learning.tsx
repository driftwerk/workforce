import { useEffect, useMemo, useState, type ReactNode } from "react";
import {
  Check,
  ChevronDown,
  ChevronRight,
  ExternalLink,
  HelpCircle,
  Network,
  Pencil,
  Play,
  Plus,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
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
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { EmptyState } from "@/components/empty-state";
import { useIdentities } from "@/hooks/use-identities";
import {
  useAgentNode,
  useAgentNodes,
  useAgentPlans,
  useApproveNode,
  useDeleteNode,
  useDeletePlan,
  useRejectNode,
  useRunPlan,
  useUpsertPlan,
} from "@/hooks/use-agent-self-learning";
import type {
  CurationMode,
  DeepNode,
  NodeFilters,
  NodeStatus,
  Plan,
  PlanUpsert,
  PlanUpsertSource,
} from "@/hooks/use-agent-self-learning";
import { ApiError } from "@/lib/api-client";
import { cn, formatRelativeTime } from "@/lib/utils";

const SLUG_PATTERN = /^[A-Za-z0-9._-]+$/;

// Friendly catalog of sources a plan can research, so the operator picks from a
// list instead of typing a skill name. `needsReadSources` flags the ones that
// only work when the agent also has read-on-demand (`[worker_read_sources]`)
// enabled; the rest are available to a curation turn out of the box.
const SOURCE_CATALOG: {
  skill: string;
  label: string;
  hint: string;
  needsReadSources?: boolean;
}[] = [
  { skill: "knowledge", label: "Codebase knowledge", hint: "This agent's indexed code graph" },
  { skill: "web", label: "Web", hint: "Public web search and pages" },
  { skill: "database", label: "Databases", hint: "Connected SQL databases (read-only)" },
  { skill: "metabase", label: "Metabase", hint: "Saved questions and dashboards" },
  { skill: "jira", label: "Jira", hint: "Issues and projects" },
  { skill: "linear", label: "Linear", hint: "Issues and projects" },
  { skill: "datadog", label: "Datadog", hint: "Logs, monitors, incidents" },
  { skill: "notion", label: "Notion", hint: "Pages and databases", needsReadSources: true },
  { skill: "github", label: "GitHub", hint: "Repos, PRs, issues", needsReadSources: true },
  { skill: "slack", label: "Slack", hint: "Channel history", needsReadSources: true },
  { skill: "google", label: "Google Drive", hint: "Docs and files", needsReadSources: true },
];

type Cadence = "manual" | "hourly" | "daily" | "weekly" | "custom";

const CADENCE_OPTIONS: { value: string; label: string }[] = [
  { value: "manual", label: "Manual — only when I click Run now" },
  { value: "hourly", label: "Hourly" },
  { value: "daily", label: "Daily (06:00)" },
  { value: "weekly", label: "Weekly (Monday 06:00)" },
  { value: "custom", label: "Custom schedule (cron)…" },
];

const ENGINE_OPTIONS: { value: string; label: string }[] = [
  { value: "default", label: "Default — use the agent's setting" },
  { value: "model", label: "Model — the agent's LLM" },
  { value: "claude_code", label: "Claude Code (uses your subscription)" },
];

const CADENCE_CRON: Record<"hourly" | "daily" | "weekly", string> = {
  hourly: "0 * * * *",
  daily: "0 6 * * *",
  weekly: "0 6 * * 1",
};

// Reverse-map a saved plan back to the friendly cadence picker.
function cadenceOf(plan: Plan | null): { cadence: Cadence; cron: string } {
  if (!plan) return { cadence: "daily", cron: "" };
  if (plan.schedule) {
    const preset = (
      Object.entries(CADENCE_CRON) as ["hourly" | "daily" | "weekly", string][]
    ).find(([, cron]) => cron === plan.schedule);
    return preset
      ? { cadence: preset[0], cron: "" }
      : { cadence: "custom", cron: plan.schedule };
  }
  if (plan.interval_secs != null) return { cadence: "custom", cron: "" };
  return { cadence: "manual", cron: "" };
}

function scheduleFromCadence(cadence: Cadence, cron: string): string | undefined {
  if (cadence === "manual") return undefined;
  if (cadence === "custom") return blankToUndefined(cron);
  return CADENCE_CRON[cadence];
}

function slugify(s: string): string {
  return s
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
}

const NODE_STATUS_VARIANTS: Record<
  NodeStatus,
  "default" | "secondary" | "destructive" | "outline" | "success" | "warning"
> = {
  approved: "success",
  verified: "default",
  modelled: "warning",
  seeded: "secondary",
  stale: "secondary",
  rejected: "outline",
  superseded: "outline",
};

const NODE_STATUS_LABELS: Record<NodeStatus, string> = {
  approved: "Approved",
  verified: "Verified",
  modelled: "Modelled",
  seeded: "Seeded",
  stale: "Stale",
  rejected: "Rejected",
  superseded: "Superseded",
};

const NODE_STATUS_FILTERS: { value: NodeStatus | "all"; label: string }[] = [
  { value: "all", label: "All statuses" },
  { value: "seeded", label: "Seeded" },
  { value: "modelled", label: "Modelled" },
  { value: "verified", label: "Verified" },
  { value: "approved", label: "Approved" },
  { value: "rejected", label: "Rejected" },
  { value: "stale", label: "Stale" },
  { value: "superseded", label: "Superseded" },
];

interface AgentSelfLearningProps {
  agentId: string;
}

export function AgentSelfLearning({ agentId }: AgentSelfLearningProps) {
  return (
    <div className="space-y-10">
      <PlansSection agentId={agentId} />
      <TreeSection agentId={agentId} />
    </div>
  );
}

function PlansSection({ agentId }: { agentId: string }) {
  const { data, isLoading, error } = useAgentPlans(agentId);
  const { data: identitiesData } = useIdentities();
  const deleteMutation = useDeletePlan(agentId);
  const runMutation = useRunPlan(agentId);
  const plans: Plan[] = useMemo(() => data?.plans ?? [], [data?.plans]);
  const identityName = useMemo(() => {
    const map = new Map<string, string>();
    for (const i of identitiesData?.identities ?? []) map.set(i.id, i.display_name);
    return map;
  }, [identitiesData?.identities]);
  const [dialogPlan, setDialogPlan] = useState<Plan | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Plan | null>(null);

  function openCreate() {
    setDialogPlan(null);
    setDialogOpen(true);
  }

  function openEdit(plan: Plan) {
    setDialogPlan(plan);
    setDialogOpen(true);
  }

  if (isLoading) {
    return <Loading />;
  }
  if (error) {
    return <SelfLearningError error={error} />;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h3 className="text-lg font-semibold">Learning plans</h3>
          <p className="text-sm text-muted-foreground">
            Scheduled, identity-scoped jobs that curate primary-cited knowledge
            from this agent's connected sources.
          </p>
        </div>
        <Button onClick={openCreate}>
          <Plus className="mr-2 h-4 w-4" />
          New plan
        </Button>
      </div>

      {plans.length === 0 ? (
        <EmptyState
          icon={Sparkles}
          title="No learning plans yet"
          description="Create a plan with topics, read-only sources, and a schedule to let this agent curate its own knowledge."
          action={{ label: "Create first plan", onClick: openCreate }}
        />
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b bg-muted/50">
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3">
                  Plan
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-32">
                  Status
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-40">
                  Schedule
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-36">
                  Run as
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-20">
                  Sources
                </th>
                <th className="text-left text-xs font-medium text-muted-foreground px-4 py-3 w-40">
                  Last run
                </th>
                <th className="text-right text-xs font-medium text-muted-foreground px-4 py-3 w-40">
                  Knowledge
                </th>
                <th className="w-40 px-4 py-3" />
              </tr>
            </thead>
            <tbody>
              {plans.map((plan) => (
                <tr key={plan.id} className="border-b last:border-0 hover:bg-muted/30">
                  <td className="px-4 py-3">
                    <div className="text-sm font-medium">{plan.name}</div>
                    <div className="text-xs font-mono text-muted-foreground">{plan.id}</div>
                  </td>
                  <td className="px-4 py-3">
                    <Badge variant={plan.enabled ? "secondary" : "outline"}>
                      {plan.enabled ? "Enabled" : "Disabled"}
                    </Badge>
                  </td>
                  <td className="px-4 py-3 font-mono text-xs text-muted-foreground">
                    {scheduleLabel(plan)}
                    {plan.enabled && plan.estimate?.warn && (
                      <div
                        className="font-sans text-warning"
                        title={`Projected ${formatUsd(plan.estimate.per_day_usd)}/day exceeds the configured daily budget threshold`}
                      >
                        ≈ {formatUsd(plan.estimate.per_week_usd)}/week
                      </div>
                    )}
                  </td>
                  <td className="px-4 py-3 text-xs">
                    <RunAsCell
                      identity={plan.run_as_identity}
                      name={
                        plan.run_as_identity
                          ? identityName.get(plan.run_as_identity)
                          : undefined
                      }
                    />
                  </td>
                  <td className="px-4 py-3 text-right text-sm tabular-nums">
                    {plan.sources.length}
                  </td>
                  <td className="px-4 py-3 text-sm text-muted-foreground">
                    {plan.last_run ? formatRelativeTime(plan.last_run) : "Never"}
                  </td>
                  <td className="px-4 py-3 text-right text-xs tabular-nums text-muted-foreground">
                    {plan.mode === "deep_expertise" ? (
                      <>
                        <span className="text-foreground">{plan.node_count}</span> nodes
                      </>
                    ) : (
                      <>
                        <span className="text-foreground">{plan.item_count}</span> total ·{" "}
                        {plan.provisional_count} prov · {plan.approved_count} appr
                      </>
                    )}
                  </td>
                  <td className="px-4 py-3">
                    <div className="flex justify-end gap-1">
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Run now"
                        disabled={runMutation.isPending}
                        onClick={() => runMutation.mutate(plan.id)}
                      >
                        <Play className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Edit plan"
                        onClick={() => openEdit(plan)}
                      >
                        <Pencil className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        title="Delete plan"
                        onClick={() => setDeleteTarget(plan)}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <PlanDialog
        agentId={agentId}
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        plan={dialogPlan}
      />

      <Dialog
        open={deleteTarget !== null}
        onOpenChange={(open) => !open && setDeleteTarget(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete plan</DialogTitle>
            <DialogDescription>
              {deleteTarget ? (
                deleteTarget.mode === "deep_expertise" ? (
                  <>
                    Removes the plan{" "}
                    <span className="font-mono">{deleteTarget.id}</span>. Its{" "}
                    {deleteTarget.node_count} knowledge-tree node
                    {deleteTarget.node_count === 1 ? "" : "s"} are kept but
                    marked orphaned. This stops future scheduled runs.
                  </>
                ) : (
                  <>
                    Removes the plan{" "}
                    <span className="font-mono">{deleteTarget.id}</span>. Its{" "}
                    {deleteTarget.item_count} curated item
                    {deleteTarget.item_count === 1 ? "" : "s"} are kept but
                    marked orphaned. This stops future scheduled runs.
                  </>
                )
              ) : null}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteTarget(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={deleteMutation.isPending}
              onClick={() => {
                if (!deleteTarget) return;
                deleteMutation.mutate(deleteTarget.id, {
                  onSuccess: () => setDeleteTarget(null),
                });
              }}
            >
              {deleteMutation.isPending ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

interface PlanDialogProps {
  agentId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  plan: Plan | null;
}

function PlanDialog({ agentId, open, onOpenChange, plan }: PlanDialogProps) {
  const editing = plan !== null;
  const { data: identitiesData, isLoading: identitiesLoading } = useIdentities();
  const identities = useMemo(
    () => identitiesData?.identities ?? [],
    [identitiesData?.identities],
  );

  const [name, setName] = useState("");
  const [body, setBody] = useState("");
  const [runAs, setRunAs] = useState("");
  const [cadence, setCadence] = useState<Cadence>("daily");
  const [cron, setCron] = useState("");
  const [skills, setSkills] = useState<Set<string>>(new Set());
  const [enabled, setEnabled] = useState(true);
  const [priority, setPriority] = useState("");
  const [maxItems, setMaxItems] = useState("");
  const [freshnessDays, setFreshnessDays] = useState("");
  const [source, setSource] = useState("default");
  const [model, setModel] = useState("");
  const [mode, setMode] = useState<CurationMode>("deep_expertise");
  const [idOverride, setIdOverride] = useState("");
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [error, setError] = useState("");
  const mutation = useUpsertPlan(agentId);

  // Repopulate the controlled fields from the target plan each time it opens.
  useEffect(() => {
    if (!open) return;
    setError("");
    mutation.reset();
    const c = cadenceOf(plan);
    setName(plan?.name ?? "");
    setBody(plan?.body ?? "");
    setRunAs(plan?.run_as_identity ?? "");
    setCadence(c.cadence);
    setCron(c.cron);
    setSkills(new Set((plan?.sources ?? []).map((s) => s.skill)));
    setEnabled(plan?.enabled ?? true);
    setPriority(plan?.priority != null ? String(plan.priority) : "");
    setMaxItems(plan?.max_items_per_run != null ? String(plan.max_items_per_run) : "");
    setFreshnessDays(plan?.freshness_days != null ? String(plan.freshness_days) : "");
    setSource(plan?.source ?? "default");
    setModel(plan?.model ?? "");
    setMode(plan?.mode ?? "deep_expertise");
    setIdOverride(plan?.id ?? "");
    setAdvancedOpen(false);
    // mutation is a stable react-query handle; only the plan identity matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, plan]);

  const planId = editing ? plan?.id ?? "" : idOverride.trim() || slugify(name);
  const idError =
    planId.length > 0 && !SLUG_PATTERN.test(planId)
      ? "ID may use letters, digits, '-', '_', '.' only."
      : null;
  const canSubmit =
    name.trim().length > 0 &&
    body.trim().length > 0 &&
    runAs.trim().length > 0 &&
    (mode === "experiential_expertise" || skills.size > 0) &&
    planId.length > 0 &&
    idError === null &&
    (cadence !== "custom" || blankToUndefined(cron) !== undefined) &&
    !mutation.isPending;

  function toggleSkill(skill: string) {
    setSkills((prev) => {
      const next = new Set(prev);
      if (next.has(skill)) next.delete(skill);
      else next.add(skill);
      return next;
    });
  }

  function handleSubmit() {
    setError("");
    // Preserve any instance/scope an advanced user set in a hand-written file.
    const original = new Map((plan?.sources ?? []).map((s) => [s.skill, s]));
    const sources: PlanUpsertSource[] = [...skills].map((skill) => {
      const prior = original.get(skill);
      return {
        skill,
        instance: prior?.instance ?? undefined,
        scope: prior?.scope ?? undefined,
      };
    });
    const payload: PlanUpsert = {
      id: planId,
      name: name.trim(),
      enabled,
      schedule: scheduleFromCadence(cadence, cron),
      interval_secs: undefined,
      run_as_identity: blankToUndefined(runAs),
      priority: parseOptionalInt(priority),
      max_items_per_run: parseOptionalInt(maxItems),
      freshness_days: parseOptionalInt(freshnessDays),
      source: source === "default" ? undefined : source,
      model: blankToUndefined(model),
      mode,
      sources,
      body: body.trim(),
    };
    mutation.mutate(payload, {
      onSuccess: () => onOpenChange(false),
      onError: (e) => setError(extractApiErrorMessage(e)),
    });
  }

  const identityOptions = useMemo(
    () => [
      { value: "", label: identitiesLoading ? "Loading people…" : "Select a person…" },
      ...identities.map((i) => ({
        value: i.id,
        label: i.email ? `${i.display_name} (${i.email})` : i.display_name,
      })),
    ],
    [identities, identitiesLoading],
  );
  const runAsUnknown = runAs.length > 0 && !identities.some((i) => i.id === runAs);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <TooltipProvider delayDuration={150}>
          <DialogHeader>
            <DialogTitle>
              {editing ? "Edit learning plan" : "New learning plan"}
            </DialogTitle>
            <DialogDescription>
              Tell the agent what to learn and where to look. It records facts for
              you to review below — you don't need to ask it to cite sources or
              flag sensitive facts, that's built in.
            </DialogDescription>
          </DialogHeader>

          <div className="max-h-[62vh] space-y-5 overflow-y-auto py-2 pr-1">
            <Field label="Name" htmlFor="sl-name">
              <Input
                id="sl-name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. Product & billing knowledge"
              />
              {!editing && planId.length > 0 && (
                <p className="text-xs text-muted-foreground">
                  ID: <span className="font-mono">{planId}</span>
                </p>
              )}
            </Field>

            <Field
              label="What should the agent learn?"
              htmlFor="sl-body"
              help="Describe the topics in plain language. The agent works out how to research them."
            >
              <textarea
                id="sl-body"
                className="w-full min-h-[96px] rounded-md border bg-background px-3 py-2 text-sm resize-y"
                value={body}
                onChange={(e) => setBody(e.target.value)}
                placeholder="e.g. How our billing tiers, pricing, trials and refunds work — and where each is documented."
              />
            </Field>

            <Field
              label="Mode"
              help="Deep expertise researches your sources into a knowledge tree. Experiential distils lessons from the agent's own past sessions (no sources needed). Items is the legacy flat-facts mode."
            >
              <FilterSelect
                value={mode}
                onChange={(v) => setMode(v as CurationMode)}
                options={[
                  { value: "deep_expertise", label: "Deep expertise (research the code/sources)" },
                  {
                    value: "experiential_expertise",
                    label: "Experiential (learn from past sessions)",
                  },
                  { value: "items", label: "Atomic facts (legacy)" },
                ]}
              />
            </Field>

            {mode !== "experiential_expertise" && (
              <Field
                label="Where should it look?"
                help="Pick the connected sources to research. A * means it also needs read-on-demand enabled on this agent."
              >
                <div className="flex flex-wrap gap-2">
                  {SOURCE_CATALOG.map((src) => {
                  const selected = skills.has(src.skill);
                  return (
                    <button
                      key={src.skill}
                      type="button"
                      onClick={() => toggleSkill(src.skill)}
                      title={src.hint}
                      className={cn(
                        "inline-flex items-center gap-1 rounded-full border px-3 py-1 text-xs transition-colors",
                        selected
                          ? "border-primary bg-primary/10 text-foreground"
                          : "border-input text-muted-foreground hover:bg-muted",
                      )}
                    >
                      {selected && <Check className="h-3 w-3" />}
                      {src.label}
                      {src.needsReadSources && <span className="text-warning">*</span>}
                    </button>
                  );
                })}
              </div>
              </Field>
            )}

            <div className="grid gap-4 sm:grid-cols-2">
              <Field
                label="Run as"
                help="The plan reads sources with this person's connected credentials. Required — a plan with no reader never runs."
              >
                <FilterSelect
                  value={runAs}
                  onChange={setRunAs}
                  options={identityOptions}
                />
                {runAsUnknown && (
                  <p className="text-xs text-warning">
                    The saved identity isn't in the list — pick a current person.
                  </p>
                )}
              </Field>

              <Field label="How often?">
                <FilterSelect
                  value={cadence}
                  onChange={(v) => setCadence(v as Cadence)}
                  options={CADENCE_OPTIONS}
                />
                {cadence === "custom" && (
                  <Input
                    value={cron}
                    onChange={(e) => setCron(e.target.value)}
                    placeholder="cron, e.g. 0 6 * * 1"
                    className="font-mono"
                  />
                )}
                {plan?.estimate && (
                  <p
                    className={cn(
                      "text-xs",
                      plan.estimate.warn ? "text-warning" : "text-muted-foreground",
                    )}
                    title={plan.estimate.basis}
                  >
                    ≈ {formatUsd(plan.estimate.per_run_usd)}/run ·{" "}
                    {formatUsd(plan.estimate.per_week_usd)}/week for the saved
                    plan
                    {plan.estimate.warn && " — above the daily budget threshold"}
                  </p>
                )}
              </Field>
            </div>

            <Toggle
              label="Plan is active"
              description="An inactive plan never runs, even on its schedule."
              checked={enabled}
              onChange={setEnabled}
            />

            <Collapsible open={advancedOpen} onOpenChange={setAdvancedOpen}>
              <CollapsibleTrigger>
                <ChevronDown
                  className={cn(
                    "h-3.5 w-3.5 transition-transform",
                    advancedOpen && "rotate-180",
                  )}
                />
                Advanced settings
              </CollapsibleTrigger>
              <CollapsibleContent className="mt-3 space-y-4">
                <div className="grid gap-4 sm:grid-cols-3">
                  <Field
                    label="Priority"
                    htmlFor="sl-priority"
                    help="When several plans are due at once, the lower number runs first. Default 100."
                  >
                    <Input
                      id="sl-priority"
                      type="number"
                      value={priority}
                      onChange={(e) => setPriority(e.target.value)}
                      placeholder="100"
                    />
                  </Field>
                  <Field
                    label="Max facts / run"
                    htmlFor="sl-maxitems"
                    help="Caps how many new facts one run records. Default 50."
                  >
                    <Input
                      id="sl-maxitems"
                      type="number"
                      min="1"
                      value={maxItems}
                      onChange={(e) => setMaxItems(e.target.value)}
                      placeholder="50"
                    />
                  </Field>
                  <Field
                    label="Freshness (days)"
                    htmlFor="sl-freshness"
                    help="A fact is re-checked against its source after this many days. Default 30."
                  >
                    <Input
                      id="sl-freshness"
                      type="number"
                      min="1"
                      value={freshnessDays}
                      onChange={(e) => setFreshnessDays(e.target.value)}
                      placeholder="30"
                    />
                  </Field>
                </div>
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field
                    label="Curation engine"
                    help="Which engine researches and records facts. Claude Code uses your Claude subscription and needs a setup token."
                  >
                    <FilterSelect
                      value={source}
                      onChange={setSource}
                      options={ENGINE_OPTIONS}
                    />
                  </Field>
                  <Field
                    label="Model"
                    htmlFor="sl-model"
                    help="Override the curation model, e.g. 'cheap', 'powerful', or a model id. Blank uses the agent's model."
                  >
                    <Input
                      id="sl-model"
                      value={model}
                      onChange={(e) => setModel(e.target.value)}
                      placeholder="cheap"
                    />
                  </Field>
                </div>
                {!editing && (
                  <Field
                    label="Plan ID"
                    htmlFor="sl-id"
                    help="Auto-derived from the name. Override only if you need a specific id."
                  >
                    <Input
                      id="sl-id"
                      value={idOverride}
                      onChange={(e) => setIdOverride(e.target.value)}
                      placeholder={slugify(name) || "product-billing"}
                      className="font-mono"
                    />
                    {idError && <p className="text-xs text-destructive">{idError}</p>}
                  </Field>
                )}
              </CollapsibleContent>
            </Collapsible>

            {error && <p className="text-sm text-destructive">{error}</p>}
          </div>

          <DialogFooter>
            <Button variant="outline" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button disabled={!canSubmit} onClick={handleSubmit}>
              {mutation.isPending ? "Saving…" : editing ? "Save plan" : "Create plan"}
            </Button>
          </DialogFooter>
        </TooltipProvider>
      </DialogContent>
    </Dialog>
  );
}

function Field({
  label,
  htmlFor,
  help,
  children,
}: {
  label: string;
  htmlFor?: string;
  help?: string;
  children: ReactNode;
}) {
  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-1.5">
        <label className="text-sm font-medium" htmlFor={htmlFor}>
          {label}
        </label>
        {help && <HelpTip text={help} />}
      </div>
      {children}
    </div>
  );
}

function HelpTip({ text }: { text: string }) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          className="text-muted-foreground hover:text-foreground"
          aria-label="Help"
        >
          <HelpCircle className="h-3.5 w-3.5" />
        </button>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs text-xs">{text}</TooltipContent>
    </Tooltip>
  );
}

function Toggle({
  label,
  description,
  checked,
  onChange,
}: {
  label: string;
  description?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      className="flex w-full items-center justify-between rounded-md border px-3 py-2 text-left"
    >
      <span>
        <span className="block text-sm font-medium">{label}</span>
        {description && (
          <span className="block text-xs text-muted-foreground">{description}</span>
        )}
      </span>
      <span
        className={cn(
          "relative h-5 w-9 shrink-0 rounded-full transition-colors",
          checked ? "bg-primary" : "bg-muted",
        )}
      >
        <span
          className={cn(
            "absolute top-0.5 h-4 w-4 rounded-full bg-background transition-transform",
            checked ? "translate-x-4" : "translate-x-0.5",
          )}
        />
      </span>
    </button>
  );
}

// The deep-expertise tree explorer: the primary self-learning surface. Roots are
// fetched eagerly (one level, filtered by plan/status); a node is expanded on
// demand, which lazily fetches its direct children and renders them as nested
// rows. The same status/Approve/Reject/Delete review actions apply at any depth.
function TreeSection({ agentId }: { agentId: string }) {
  const { data: plansData } = useAgentPlans(agentId);
  const plans: Plan[] = useMemo(() => plansData?.plans ?? [], [plansData?.plans]);
  const [status, setStatus] = useState<NodeStatus | "all">("all");
  const [planId, setPlanId] = useState<string>("all");

  const filters: NodeFilters = useMemo(() => {
    const f: NodeFilters = {};
    if (status !== "all") f.status = status;
    if (planId !== "all") f.plan = planId;
    return f;
  }, [status, planId]);

  const { data, isLoading, error } = useAgentNodes(agentId, filters);
  const roots: DeepNode[] = useMemo(() => data?.nodes ?? [], [data?.nodes]);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold">Deep expertise</h3>
          <p className="text-sm text-muted-foreground">
            The knowledge tree the agent builds — each node is a synthesized,
            source-grounded write-up of one sub-domain. Expand a node to read it
            and review its children.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <FilterSelect
            value={status}
            onChange={(v) => setStatus(v as NodeStatus | "all")}
            options={NODE_STATUS_FILTERS}
          />
          <FilterSelect
            value={planId}
            onChange={setPlanId}
            options={[
              { value: "all", label: "All plans" },
              ...plans.map((p) => ({ value: p.id, label: p.name })),
            ]}
          />
        </div>
      </div>

      {isLoading ? (
        <Loading />
      ) : error ? (
        <SelfLearningError error={error} />
      ) : roots.length === 0 ? (
        <EmptyState
          icon={Network}
          title="No knowledge tree yet"
          description="When a learning plan runs it builds a tree of source-grounded expertise here for review. Adjust the filters or run a plan to populate it."
        />
      ) : (
        <div className="space-y-2">
          {roots.map((node) => (
            <NodeRow key={node.id} agentId={agentId} node={node} />
          ))}
        </div>
      )}
    </div>
  );
}

function NodeRow({ agentId, node }: { agentId: string; node: DeepNode }) {
  const [expanded, setExpanded] = useState(false);
  const [bodyOpen, setBodyOpen] = useState(false);
  const [noteOpen, setNoteOpen] = useState(false);
  const [rejectOpen, setRejectOpen] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);

  const detail = useAgentNode(agentId, node.id, expanded);
  const children: DeepNode[] = detail.data?.children ?? [];

  const approve = useApproveNode(agentId);
  const deleteNode = useDeleteNode(agentId);

  // Children render nested inside the expanded container (see below), which is
  // indented once via `border-l`, so the hierarchy steps uniformly per level
  // without a per-row offset.
  return (
    <div>
      <div className="rounded-lg border">
        <div className="flex items-start gap-2 px-3 py-2.5">
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="mt-0.5 text-muted-foreground hover:text-foreground"
            aria-label={expanded ? "Collapse children" : "Expand children"}
          >
            {expanded ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronRight className="h-4 w-4" />
            )}
          </button>
          <div className="min-w-0 flex-1 space-y-1">
            <div className="flex flex-wrap items-center gap-2">
              <NodeStatusBadge status={node.status} />
              <span className="text-sm font-medium">{node.title}</span>
              <span className="text-xs text-muted-foreground">
                depth {node.depth} · {formatConfidence(node.confidence)}
              </span>
            </div>
            {node.review_note && (
              <Collapsible open={noteOpen} onOpenChange={setNoteOpen}>
                <CollapsibleTrigger className="text-xs text-warning">
                  <ChevronDown
                    className={cn(
                      "h-3.5 w-3.5 transition-transform",
                      noteOpen && "rotate-180",
                    )}
                  />
                  {noteOpen ? "Hide grounding note" : "Claims to double-check"}
                </CollapsibleTrigger>
                <CollapsibleContent className="mt-1">
                  <p className="whitespace-pre-wrap text-xs text-muted-foreground">
                    {node.review_note}
                  </p>
                </CollapsibleContent>
              </Collapsible>
            )}
            <Collapsible open={bodyOpen} onOpenChange={setBodyOpen}>
              <CollapsibleTrigger>
                <ChevronDown
                  className={cn(
                    "h-3.5 w-3.5 transition-transform",
                    bodyOpen && "rotate-180",
                  )}
                />
                {bodyOpen ? "Hide write-up" : "Read write-up"}
              </CollapsibleTrigger>
              <CollapsibleContent className="mt-2 space-y-3">
                <p className="whitespace-pre-wrap text-sm text-foreground">
                  {node.body}
                </p>
                <NodeSources sources={node.sources} />
              </CollapsibleContent>
            </Collapsible>
          </div>
          <div className="flex shrink-0 gap-1">
            <Button
              variant="ghost"
              size="sm"
              title="Approve"
              disabled={node.status === "approved" || approve.isPending}
              onClick={() => approve.mutate(node.id)}
            >
              <Check className="h-4 w-4" />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              title="Reject"
              disabled={node.status === "rejected"}
              onClick={() => setRejectOpen(true)}
            >
              <X className="h-4 w-4" />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              title="Delete subtree"
              onClick={() => setDeleteOpen(true)}
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        </div>
      </div>

      {expanded && (
        <div className="ml-3 mt-2 space-y-2 border-l pl-3">
          {detail.isLoading ? (
            <Loading />
          ) : detail.error ? (
            <SelfLearningError error={detail.error} />
          ) : children.length === 0 ? (
            <p className="text-xs text-muted-foreground">No sub-topics.</p>
          ) : (
            children.map((child) => (
              <NodeRow key={child.id} agentId={agentId} node={child} />
            ))
          )}
        </div>
      )}

      <NodeRejectDialog
        agentId={agentId}
        node={rejectOpen ? node : null}
        onClose={() => setRejectOpen(false)}
      />

      <Dialog open={deleteOpen} onOpenChange={setDeleteOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete subtree</DialogTitle>
            <DialogDescription>
              Permanently removes{" "}
              <span className="font-medium">{node.title}</span> and every node
              beneath it. The agent immediately stops surfacing them.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteOpen(false)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={deleteNode.isPending}
              onClick={() =>
                deleteNode.mutate(node.id, {
                  onSuccess: () => setDeleteOpen(false),
                })
              }
            >
              {deleteNode.isPending ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function NodeSources({ sources }: { sources: DeepNode["sources"] }) {
  if (sources.length === 0) {
    return <p className="text-xs text-muted-foreground">No sources cited.</p>;
  }
  return (
    <div className="space-y-1">
      <div className="text-xs font-medium text-muted-foreground">Sources</div>
      <ul className="space-y-0.5">
        {sources.map((src, i) => {
          const url = isHttpUrl(src.reference) ? src.reference : null;
          return (
            <li key={`${src.reference}-${i}`} className="flex items-center gap-1.5 text-xs">
              {src.skill && (
                <Badge variant="outline" className="font-mono text-[0.65rem]">
                  {src.skill}
                </Badge>
              )}
              {url ? (
                <a
                  href={url}
                  target="_blank"
                  rel="noreferrer"
                  className="flex items-center gap-1 text-primary hover:underline"
                >
                  <span className="truncate font-mono">{src.reference}</span>
                  <ExternalLink className="h-3 w-3 shrink-0" />
                </a>
              ) : (
                <span className="truncate font-mono text-muted-foreground">
                  {src.reference}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function NodeRejectDialog({
  agentId,
  node,
  onClose,
}: {
  agentId: string;
  node: DeepNode | null;
  onClose: () => void;
}) {
  const [note, setNote] = useState("");
  const reject = useRejectNode(agentId);

  useEffect(() => {
    if (node) {
      setNote("");
      reject.reset();
    }
    // Only reset when the target node changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [node]);

  return (
    <Dialog open={node !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Reject node</DialogTitle>
          <DialogDescription>
            Marks this node rejected so it stops surfacing. Add an optional
            reason for the curation log.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2 py-2">
          <textarea
            className="w-full min-h-[80px] rounded-md border bg-background px-3 py-2 text-sm resize-y"
            value={note}
            onChange={(e) => setNote(e.target.value)}
            placeholder="Why is this node wrong? (optional)"
          />
          {reject.isError && (
            <p className="text-sm text-destructive">
              {extractApiErrorMessage(reject.error)}
            </p>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="destructive"
            disabled={reject.isPending}
            onClick={() => {
              if (!node) return;
              reject.mutate(
                { nodeId: node.id, note: blankToUndefined(note) },
                { onSuccess: onClose },
              );
            }}
          >
            {reject.isPending ? "Rejecting…" : "Reject"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function NodeStatusBadge({ status }: { status: NodeStatus }) {
  return (
    <Badge variant={NODE_STATUS_VARIANTS[status]}>
      {NODE_STATUS_LABELS[status]}
    </Badge>
  );
}

// A plan with no run-as identity never runs (design §5.1), so flag the absence
// instead of rendering a neutral dash. Resolves to the person's name when known.
function RunAsCell({ identity, name }: { identity: string | null; name?: string }) {
  if (!identity) {
    return <span className="text-warning">unset — inert</span>;
  }
  return (
    <span className="text-muted-foreground" title={identity}>
      {name ?? `${identity.slice(0, 8)}…`}
    </span>
  );
}

function FilterSelect({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (value: string) => void;
  options: { value: string; label: string }[];
}) {
  return (
    <select
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className="h-9 rounded-md border border-input bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
    >
      {options.map((opt) => (
        <option key={opt.value} value={opt.value}>
          {opt.label}
        </option>
      ))}
    </select>
  );
}

function Loading() {
  return (
    <div className="py-12 text-center text-sm text-muted-foreground">Loading…</div>
  );
}

function SelfLearningError({ error }: { error: unknown }) {
  const message = extractApiErrorMessage(error);
  const status = error instanceof ApiError ? error.status : 0;
  if (status === 503) {
    return (
      <div className="rounded-md border border-warning/30 bg-warning/10 p-4 text-sm">
        Self-learning is disabled for this hub. Enable{" "}
        <span className="font-mono">[self_learning] enabled</span> in{" "}
        <span className="font-mono">workforce.toml</span> or via the per-agent
        override.
      </div>
    );
  }
  return (
    <div className="rounded-md border border-destructive/30 bg-destructive/10 p-4 text-sm text-destructive">
      Failed to load self-learning data: {message}
    </div>
  );
}

function scheduleLabel(plan: Plan): string {
  if (plan.schedule) {
    const preset = (
      Object.entries(CADENCE_CRON) as ["hourly" | "daily" | "weekly", string][]
    ).find(([, c]) => c === plan.schedule);
    if (preset) return preset[0].charAt(0).toUpperCase() + preset[0].slice(1);
    return plan.schedule;
  }
  if (plan.interval_secs != null) return `every ${plan.interval_secs}s`;
  return "Manual";
}

function formatConfidence(confidence: number): string {
  if (!Number.isFinite(confidence)) return "—";
  return `${Math.round(confidence * 100)}%`;
}

function formatUsd(amount: number): string {
  if (!Number.isFinite(amount)) return "—";
  return `$${amount.toFixed(2)}`;
}

function isHttpUrl(s: string): boolean {
  return /^https?:\/\//i.test(s);
}

function blankToUndefined(s: string): string | undefined {
  const trimmed = s.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function parseOptionalInt(s: string): number | undefined {
  const trimmed = s.trim();
  if (trimmed.length === 0) return undefined;
  const n = parseInt(trimmed, 10);
  return Number.isNaN(n) ? undefined : n;
}

function extractApiErrorMessage(error: unknown): string {
  if (!error) return "Unknown error";
  if (typeof error === "string") return error;
  if (error instanceof Error) {
    try {
      const parsed = JSON.parse(error.message) as { error?: string };
      if (parsed.error) return parsed.error;
    } catch {
      // not JSON — fall through to raw message
    }
    return error.message;
  }
  return "Unknown error";
}
