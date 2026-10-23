import { useState, type ReactNode } from "react";
import { AlertTriangle, Boxes, Info, Plus, Star, Trash2, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { EmptyState } from "@/components/empty-state";
import { useAgentEnvironments, useSaveAgentEnvironments } from "@/hooks/use-environments";
import { ApiError } from "@/lib/api-client";
import { apiErrorMessage } from "@/lib/api-error";
import type { EnvironmentOptions, EnvironmentView, McpBindings, SkillBindings } from "@/lib/api/environments";
import {
  toBody,
  withDefault,
  withEnabled,
  withEnvironment,
  withMcp,
  withoutEnvironment,
  withoutMcp,
  withoutSkill,
  withoutSkillInstance,
  withPromoted,
  withSkill,
  type Change,
} from "@/lib/environments";

const SELECT_CLASS =
  "h-9 rounded-md border bg-background px-2 text-sm focus:outline-none focus:ring-2 focus:ring-ring";

type Apply = (change: Change, onSuccess?: () => void) => void;

function SaveError({ error }: { error: Error | null }) {
  if (!error) return null;
  const stale = error instanceof ApiError && error.status === 409;
  return (
    <p role="alert" className="text-sm text-destructive">
      {stale
        ? "Not saved: this agent's config changed meanwhile. The current environments are shown; make the change again."
        : `Not saved: ${apiErrorMessage(error)}`}
    </p>
  );
}

interface AgentEnvironmentsProps {
  agentId: string;
}

export function AgentEnvironments({ agentId }: AgentEnvironmentsProps) {
  const { data, isLoading, isFetching, error } = useAgentEnvironments(agentId);
  const save = useSaveAgentEnvironments(agentId);
  const [createOpen, setCreateOpen] = useState(false);
  const [deleting, setDeleting] = useState<EnvironmentView | null>(null);

  if (isLoading) {
    return <div className="py-12 text-center text-sm text-muted-foreground">Loading…</div>;
  }
  if (error || !data) {
    return (
      <div className="rounded-md border border-destructive/30 bg-destructive/10 p-3 text-sm text-destructive">
        Failed to load environments: {apiErrorMessage(error)}
      </div>
    );
  }

  const readOnly = !data.allowed;
  const busy = readOnly || save.isPending || isFetching;
  const apply: Apply = (change, onSuccess) =>
    save.mutate({ etag: data.etag, body: change(toBody(data)) }, { onSuccess });
  const openCreate = () => {
    save.reset();
    setCreateOpen(true);
  };
  const openDelete = (env: EnvironmentView) => {
    save.reset();
    setDeleting(env);
  };

  return (
    <div className="space-y-4">
      <div className="flex items-start justify-between gap-4 flex-wrap">
        <div>
          <h3 className="text-lg font-semibold">Environments</h3>
          <p className="text-sm text-muted-foreground max-w-2xl">
            Named bundles that pin this agent's multi-instance skills and MCP workspaces to one
            coherent target, so it never mixes environments in a single answer. The default is
            active in every session that hasn't switched with <code>cmd: env use</code>. Other
            agents never see these.
          </p>
        </div>
        <Button onClick={openCreate} disabled={busy}>
          <Plus className="h-4 w-4 mr-1.5" />
          New Environment
        </Button>
      </div>

      {readOnly && (
        <div role="status" className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm">
          <AlertTriangle aria-hidden className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
          <span>
            Environments are turned off for this install (<code>environments.enabled = false</code>),
            so no agent uses them. Shown read-only.
          </span>
        </div>
      )}

      {save.error && !createOpen && !deleting && (
        <div className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2">
          <SaveError error={save.error} />
        </div>
      )}

      {save.isSuccess && (save.data.notes ?? []).length > 0 && (
        <ul role="status" className="space-y-1 rounded-md border px-3 py-2 text-sm text-muted-foreground">
          {(save.data.notes ?? []).map((note) => (
            <li key={note} className="flex items-start gap-2">
              <Info aria-hidden className="mt-0.5 h-4 w-4 shrink-0" />
              {note}
            </li>
          ))}
        </ul>
      )}

      <div className="flex flex-wrap items-center gap-6 rounded-md border px-4 py-3">
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={data.enabled}
            disabled={busy}
            onChange={(event) => apply(withEnabled(event.target.checked))}
          />
          Use environments on this agent
        </label>
        <label className="flex items-center gap-2 text-sm">
          Default
          <select
            className={SELECT_CLASS}
            value={data.default ?? ""}
            disabled={busy}
            onChange={(event) => apply(withDefault(event.target.value || null))}
          >
            <option value="">None</option>
            {data.environments.map((env) => (
              <option key={env.name} value={env.name}>
                {env.draft ? `${env.name} (draft, approves it)` : env.name}
              </option>
            ))}
          </select>
        </label>
      </div>

      {!data.enabled && data.environments.length > 0 && (
        <p className="text-sm text-muted-foreground">
          Off: this agent ignores these environments until they are turned on.
        </p>
      )}

      {data.environments.length === 0 ? (
        <EmptyState
          icon={Boxes}
          title="No environments"
          description="Create an environment, then pin skills and MCP workspaces to one target."
          action={readOnly ? undefined : { label: "New Environment", onClick: openCreate }}
        />
      ) : (
        <div className="space-y-4">
          {data.environments.map((env) => (
            <EnvironmentCard
              key={env.name}
              env={env}
              options={data.options}
              busy={busy}
              apply={apply}
              onDelete={() => openDelete(env)}
            />
          ))}
        </div>
      )}

      <CreateEnvironmentDialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        taken={data.environments.map((env) => env.name)}
        busy={busy}
        error={save.error}
        onCreate={(name, description, done) => apply(withEnvironment(name, description), done)}
      />

      <Dialog open={deleting !== null} onOpenChange={(open) => !open && setDeleting(null)}>
        <DialogContent>
          {deleting && (
            <>
              <DialogHeader>
                <DialogTitle>Delete environment</DialogTitle>
                <DialogDescription>
                  Removes <span className="font-mono">{deleting.name}</span> and its bindings from this
                  agent.
                  {deleting.default && " It is the default, so the agent is left with no default environment."}
                </DialogDescription>
              </DialogHeader>
              <SaveError error={save.error} />
              <DialogFooter>
                <Button variant="outline" onClick={() => setDeleting(null)}>
                  Cancel
                </Button>
                <Button
                  variant="destructive"
                  disabled={busy}
                  onClick={() => apply(withoutEnvironment(deleting.name), () => setDeleting(null))}
                >
                  Delete
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}

interface CreateEnvironmentDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  taken: string[];
  busy: boolean;
  error: Error | null;
  onCreate: (name: string, description: string, done: () => void) => void;
}

function CreateEnvironmentDialog({ open, onOpenChange, taken, busy, error, onCreate }: CreateEnvironmentDialogProps) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const trimmed = name.trim();
  const duplicate = taken.includes(trimmed);

  const changeOpen = (next: boolean) => {
    if (!next) {
      setName("");
      setDescription("");
    }
    onOpenChange(next);
  };

  return (
    <Dialog open={open} onOpenChange={changeOpen}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New environment</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <Input
            placeholder="Name, e.g. acme-prod"
            value={name}
            onChange={(e) => setName(e.target.value)}
            aria-label="Environment name"
          />
          {duplicate && (
            <p className="text-xs text-destructive">This agent already has an environment named {trimmed}.</p>
          )}
          <Input
            placeholder="Description (optional)"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            aria-label="Environment description"
          />
          <SaveError error={error} />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => changeOpen(false)}>
            Cancel
          </Button>
          <Button onClick={() => onCreate(trimmed, description, () => changeOpen(false))} disabled={!trimmed || duplicate || busy}>
            Create
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface EnvironmentCardProps {
  env: EnvironmentView;
  options: EnvironmentOptions;
  busy: boolean;
  apply: Apply;
  onDelete: () => void;
}

function EnvironmentCard({ env, options, busy, apply, onDelete }: EnvironmentCardProps) {
  const skills = Object.entries(env.skills);
  const mcp = Object.entries(env.mcp);
  const nothingToBind = options.skills.length === 0 && options.mcp_workspaces.length === 0;

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <CardTitle className="text-base">{env.name}</CardTitle>
          {env.default && (
            <Badge variant="secondary">
              <Star className="h-3 w-3 mr-1" />
              Default
            </Badge>
          )}
          {env.draft && <Badge variant="warning">Draft</Badge>}
        </div>
        <div className="flex items-center gap-1">
          {env.draft && (
            <Button size="sm" onClick={() => apply(withPromoted(env.name))} disabled={busy}>
              Approve
            </Button>
          )}
          {!env.default && (
            <Button
              size="sm"
              variant="outline"
              onClick={() => apply(withDefault(env.name))}
              disabled={busy}
            >
              {env.draft ? "Approve and make default" : "Make default"}
            </Button>
          )}
          <Button
            size="sm"
            variant="ghost"
            className="h-8 text-destructive"
            disabled={busy}
            onClick={onDelete}
            aria-label={`Delete ${env.name}`}
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {env.description && <p className="text-sm text-muted-foreground">{env.description}</p>}
        {env.draft && (
          <p className="text-sm text-muted-foreground">
            The agent proposed this environment. No session uses it until it is approved.
          </p>
        )}

        {skills.length + mcp.length === 0 ? (
          <p className="text-sm text-muted-foreground">No bindings — this environment pins nothing yet.</p>
        ) : (
          <div className="space-y-2">
            {skills.map(([skill, instances]) => (
              <BindingRow
                key={`skill/${skill}`}
                kind="skill"
                name={skill}
                busy={busy}
                onRemove={() => apply(withoutSkill(env.name, skill))}
              >
                <InstanceChips
                  instances={instances}
                  disabled={busy}
                  onRemove={(instance) => apply(withoutSkillInstance(env.name, skill, instance))}
                />
              </BindingRow>
            ))}
            {mcp.map(([workspace, variant]) => (
              <BindingRow
                key={`mcp/${workspace}`}
                kind="mcp"
                name={workspace}
                busy={busy}
                onRemove={() => apply(withoutMcp(env.name, workspace))}
              >
                <span className="font-mono text-sm">{variant}</span>
              </BindingRow>
            ))}
          </div>
        )}

        {nothingToBind ? (
          <p className="text-sm text-muted-foreground">
            This agent has no multi-instance skills or MCP workspace variants to bind.
          </p>
        ) : (
          <>
            <SkillBindingForm
              options={options}
              bound={env.skills}
              busy={busy}
              onBind={(skill, instances, done) => apply(withSkill(env.name, skill, instances), done)}
            />
            <McpBindingForm
              options={options}
              bound={env.mcp}
              busy={busy}
              onBind={(workspace, variant, done) => apply(withMcp(env.name, workspace, variant), done)}
            />
          </>
        )}

        {env.warnings.length > 0 && (
          <div className="space-y-1">
            {env.warnings.map((w) => (
              <p key={w} className="flex items-start gap-1.5 text-xs text-warning">
                <AlertTriangle aria-hidden className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                {w}
              </p>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

interface BindingRowProps {
  kind: "skill" | "mcp";
  name: string;
  busy: boolean;
  onRemove: () => void;
  children: ReactNode;
}

function BindingRow({ kind, name, busy, onRemove, children }: BindingRowProps) {
  return (
    <div className="flex items-center justify-between gap-2 rounded-md border px-3 py-2">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <Badge variant="outline">{kind}</Badge>
        <span className="font-medium">{name}</span>
        <span className="text-muted-foreground">→</span>
        {children}
      </div>
      <Button size="sm" variant="ghost" className="h-7 text-xs text-destructive" onClick={onRemove} disabled={busy}>
        Remove
      </Button>
    </div>
  );
}

interface InstanceChipsProps {
  instances: string[];
  disabled: boolean;
  onRemove?: (instance: string) => void;
}

function InstanceChips({ instances, disabled, onRemove }: InstanceChipsProps) {
  return (
    <span className="flex flex-wrap items-center gap-1">
      {instances.map((instance, index) => (
        <Badge
          key={instance}
          variant={index === 0 ? "secondary" : "outline"}
          className="font-mono"
          title={index === 0 ? "Default instance" : undefined}
        >
          {index === 0 && <Star className="h-3 w-3 mr-1" />}
          {instance}
          {onRemove && (
            <button
              type="button"
              className="ml-1 rounded-sm hover:text-destructive disabled:opacity-50"
              onClick={() => onRemove(instance)}
              disabled={disabled}
              aria-label={`Remove ${instance}`}
            >
              <X className="h-3 w-3" />
            </button>
          )}
        </Badge>
      ))}
    </span>
  );
}

interface SkillBindingFormProps {
  options: EnvironmentOptions;
  bound: SkillBindings;
  busy: boolean;
  onBind: (skill: string, instances: string[], done: () => void) => void;
}

function SkillBindingForm({ options, bound, busy, onBind }: SkillBindingFormProps) {
  const [skill, setSkill] = useState("");
  const [picked, setPicked] = useState<string[]>([]);
  const remaining = (options.skills.find((s) => s.name === skill)?.instances ?? []).filter(
    (instance) => !picked.includes(instance),
  );

  if (options.skills.length === 0) return null;

  const reset = () => {
    setSkill("");
    setPicked([]);
  };

  return (
    <div className="flex flex-wrap items-center gap-2 rounded-md border bg-muted/30 p-3">
      <select
        className={SELECT_CLASS}
        value={skill}
        onChange={(e) => {
          setSkill(e.target.value);
          setPicked([]);
        }}
        disabled={busy}
        aria-label="Skill"
      >
        <option value="">Select skill…</option>
        {options.skills.map((s) => (
          <option key={s.name} value={s.name}>
            {s.name}
          </option>
        ))}
      </select>
      <select
        className={SELECT_CLASS}
        value=""
        onChange={(e) => setPicked([...picked, e.target.value])}
        disabled={busy || remaining.length === 0}
        aria-label="Add instance"
      >
        <option value="">{picked.length === 0 ? "Default instance…" : "Then…"}</option>
        {remaining.map((instance) => (
          <option key={instance} value={instance}>
            {instance}
          </option>
        ))}
      </select>
      <InstanceChips
        instances={picked}
        disabled={busy}
        onRemove={(instance) => setPicked(picked.filter((i) => i !== instance))}
      />
      <Button size="sm" onClick={() => onBind(skill, picked, reset)} disabled={!skill || picked.length === 0 || busy}>
        <Plus className="h-3.5 w-3.5 mr-1" />
        {Object.hasOwn(bound, skill) ? "Replace" : "Bind"}
      </Button>
    </div>
  );
}

interface McpBindingFormProps {
  options: EnvironmentOptions;
  bound: McpBindings;
  busy: boolean;
  onBind: (workspace: string, variant: string, done: () => void) => void;
}

function McpBindingForm({ options, bound, busy, onBind }: McpBindingFormProps) {
  const [workspace, setWorkspace] = useState("");
  const [variant, setVariant] = useState("");
  const variants = options.mcp_workspaces.find((w) => w.name === workspace)?.envs ?? [];

  if (options.mcp_workspaces.length === 0) return null;

  return (
    <div className="flex flex-wrap items-center gap-2 rounded-md border bg-muted/30 p-3">
      <select
        className={SELECT_CLASS}
        value={workspace}
        onChange={(e) => {
          setWorkspace(e.target.value);
          setVariant("");
        }}
        disabled={busy}
        aria-label="MCP workspace"
      >
        <option value="">Select MCP workspace…</option>
        {options.mcp_workspaces.map((w) => (
          <option key={w.name} value={w.name}>
            {w.name}
          </option>
        ))}
      </select>
      <select
        className={SELECT_CLASS}
        value={variant}
        onChange={(e) => setVariant(e.target.value)}
        disabled={busy || !workspace}
        aria-label="MCP environment"
      >
        <option value="">Select environment…</option>
        {variants.map((v) => (
          <option key={v} value={v}>
            {v}
          </option>
        ))}
      </select>
      <Button
        size="sm"
        onClick={() =>
          onBind(workspace, variant, () => {
            setWorkspace("");
            setVariant("");
          })
        }
        disabled={!workspace || !variant || busy}
      >
        <Plus className="h-3.5 w-3.5 mr-1" />
        {Object.hasOwn(bound, workspace) ? "Replace" : "Bind"}
      </Button>
    </div>
  );
}
