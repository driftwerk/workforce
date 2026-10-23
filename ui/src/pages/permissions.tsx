import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import {
  useCreateCustomRole,
  useUpdateCustomRole,
  useDeleteCustomRole,
  usePermissionRules,
  useUpdatePermissionRule,
  useDeletePermissionRule,
  usePermissionAudit,
  useCapabilityMatrix,
  useUpdateCapabilityMatrix,
  useCapabilityAudit,
  useCapabilityCatalog,
  useEffectivePermission,
} from "@/hooks/use-permissions";
import type {
  CapabilityCatalogEntry,
  CapabilityChange,
  CapabilityDecisionValue,
  EffectiveQuery,
  PermissionRule,
  PermissionScope,
  PermissionState,
  RoleRow,
  RuleFilter,
} from "@/lib/api/permissions";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  AlertCircle,
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  Loader2,
  Lock,
  Pencil,
  Plus,
  RotateCcw,
  Shield,
  Trash2,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { useDebounced } from "@/hooks/use-debounced";
import {
  EntityCombobox,
  EntityMultiCombobox,
  useScopeLabeler,
  type ScopeLabel,
} from "@/components/permissions/entity-picker";
import { ToolCombobox } from "@/components/permissions/tool-picker";
import { CapabilityInfo } from "@/components/permissions/capability-info";
import {
  DecisionBadge,
  humanizeKey,
  roleLabelFallback,
} from "@/components/permissions/labels";
import {
  RuleForm,
  splitApprovers,
  type RuleFormValues,
} from "@/components/permissions/rule-form";

const STATE_TONE: Record<
  PermissionState,
  "default" | "secondary" | "destructive" | "outline"
> = {
  allow: "secondary",
  ask: "outline",
  always_ask: "outline",
  deny: "destructive",
};

function StateBadge({ state }: { state: PermissionState | null | undefined }) {
  if (!state) return <span className="text-muted-foreground">—</span>;
  return <Badge variant={STATE_TONE[state]}>{state}</Badge>;
}

function CascadeDisabledNotice({ error }: { error: Error }) {
  const msg = error.message ?? "Unknown error";
  const looksDisabled = /503|service unavailable|disabled/i.test(msg);
  return (
    <Card>
      <CardContent className="flex items-start gap-2 p-4 text-sm">
        <AlertCircle className="mt-0.5 h-4 w-4 text-amber-500" />
        <div>
          <div className="font-medium">
            {looksDisabled
              ? "The rules layer is not enabled"
              : "Failed to load"}
          </div>
          <div className="text-xs text-muted-foreground">
            {looksDisabled
              ? "Set [permissions] enabled = true in workforce.toml to surface the optional rules overlay."
              : msg}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

/** Neutral load-failure card for endpoints unrelated to the rules engine. */
function LoadErrorNotice({ error }: { error: Error }) {
  return (
    <Card>
      <CardContent className="flex items-start gap-2 p-4 text-sm">
        <AlertCircle className="mt-0.5 h-4 w-4 text-amber-500" />
        <div>
          <div className="font-medium">Failed to load</div>
          <div className="text-xs text-muted-foreground">
            {error.message || "Unknown error"}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

/** Nullable decision — built-in defaults have no stored override. */
function OptionalDecisionBadge({
  decision,
}: {
  decision: CapabilityDecisionValue | null;
}) {
  if (!decision) return <span className="text-muted-foreground">default</span>;
  return <DecisionBadge decision={decision} />;
}

const DECISION_CHOICES: CapabilityDecisionValue[] = ["allow", "ask", "deny"];

// Looseness ordering: deny is strictest, allow is loosest. A "grant" moves a
// cell toward a looser decision; a "removal" moves it stricter.
const DECISION_RANK: Record<CapabilityDecisionValue, number> = {
  deny: 0,
  ask: 1,
  allow: 2,
};

// High-blast-radius capabilities — loosening any of these is an escalation that
// requires typed confirmation.
const ESCALATION_CAPABILITIES = new Set<string>([
  "shell_exec",
  "vault_shared_delete",
  "database_write",
  "network_write",
  "devices_manage",
  "permissions_manage",
  "roles_assign",
]);

const DANGER_PHRASE = "CONFIRM";

interface CapabilityChangeView {
  role: string;
  capability: string;
  original: CapabilityDecisionValue;
  next: CapabilityDecisionValue;
}

/** Loosening a high-blast-radius capability — "grant". */
function isEscalation(change: CapabilityChangeView): boolean {
  return (
    ESCALATION_CAPABILITIES.has(change.capability) &&
    DECISION_RANK[change.next] > DECISION_RANK[change.original]
  );
}

/** A member capability that was allowed and is now restricted — the failure
 * mode the matrix exists to prevent. Judged against the original member value. */
function isMassRevocation(change: CapabilityChangeView): boolean {
  return (
    change.role === "member" &&
    change.original === "allow" &&
    change.next !== "allow"
  );
}

/** Tightening an admin's own permission/role management — a self-lockout only
 * the CLI break-glass can undo, so it warrants the same typed confirmation. */
function isLockout(change: CapabilityChangeView): boolean {
  return (
    change.role === "system_admin" &&
    (change.capability === "permissions_manage" ||
      change.capability === "roles_assign") &&
    DECISION_RANK[change.next] < DECISION_RANK[change.original]
  );
}

// The audit `decision` is free-form (allow / deny_unscoped / deny_owner_mismatch
// — see AuditDecision in crates/memory/src/access_audit.rs), NOT the clean
// allow/ask/deny union. Tone known denials destructive, allow secondary, and
// fall back to outline so an unexpected value still renders legibly.
function auditDecisionTone(
  decision: string,
): "secondary" | "outline" | "destructive" {
  const d = decision.toLowerCase();
  if (d.startsWith("deny") || d.startsWith("denied")) return "destructive";
  if (d.startsWith("allow")) return "secondary";
  return "outline";
}

function AuditDecisionBadge({ decision }: { decision: string }) {
  return <Badge variant={auditDecisionTone(decision)}>{decision}</Badge>;
}

const KIND_LABEL: Record<PermissionScope, string> = {
  user: "Person",
  agent: "Agent",
  team: "Team",
};

function EditRuleDialog({
  rule,
  subjectLabel,
}: {
  rule: PermissionRule;
  subjectLabel?: string;
}) {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const update = useUpdatePermissionRule();
  const initialApprovers = splitApprovers(rule.approvers);

  const handleSubmit = (values: RuleFormValues) => {
    setError(null);
    update.mutate(
      {
        id: rule.id,
        body: {
          tool_pattern: values.patterns[0],
          state: values.state,
          approvers: values.approvers,
          allow_self_approval: values.allowSelfApproval,
        },
      },
      {
        onSuccess: () => setOpen(false),
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (!o) setError(null);
      }}
    >
      <DialogTrigger asChild>
        <Button size="sm" variant="ghost" aria-label="Edit rule">
          <Pencil className="h-3.5 w-3.5" />
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[85vh] max-w-lg overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Edit permission rule</DialogTitle>
        </DialogHeader>
        <RuleForm
          mode="edit"
          subjectLabel={subjectLabel}
          initial={{
            scope: rule.scope,
            scopeId: rule.scope_id,
            scopeLabel: subjectLabel,
            patterns: [rule.tool_pattern],
            state: rule.state,
            approverUserIds: initialApprovers.userIds,
            approverRoles: initialApprovers.roles,
            allowSelfApproval: rule.allow_self_approval ?? true,
          }}
          submitLabel="Save"
          submitting={update.isPending}
          error={error}
          onSubmit={handleSubmit}
          onCancel={() => setOpen(false)}
        />
      </DialogContent>
    </Dialog>
  );
}

function DeleteRuleButton({
  rule,
  subjectLabel,
}: {
  rule: PermissionRule;
  subjectLabel?: string;
}) {
  const del = useDeletePermissionRule();
  const onDelete = () => {
    if (
      window.confirm(
        `Delete the "${rule.tool_pattern}" rule for ${KIND_LABEL[rule.scope]} ${
          subjectLabel ?? rule.scope_id
        }?`,
      )
    ) {
      del.mutate(rule.id);
    }
  };
  return (
    <Button
      size="sm"
      variant="ghost"
      onClick={onDelete}
      disabled={del.isPending}
      aria-label="Delete rule"
    >
      {del.isPending ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" />
      ) : (
        <Trash2 className="h-3.5 w-3.5 text-destructive" />
      )}
    </Button>
  );
}

function approverLabel(
  ref: import("@/lib/api/permissions").ApproverRef,
  labeler: (scope: PermissionScope, id: string) => ScopeLabel,
): string {
  return ref.kind === "role"
    ? roleLabelFallback(ref.role)
    : labeler("user", ref.id).label;
}

function ApproversBadge({
  rule,
  labeler,
}: {
  rule: PermissionRule;
  labeler: (scope: PermissionScope, id: string) => ScopeLabel;
}) {
  const approvers = rule.approvers ?? [];
  if (approvers.length === 0) return null;
  const names = approvers.map((a) => approverLabel(a, labeler)).join(", ");
  return (
    <div className="flex flex-wrap items-center gap-1">
      <Badge variant="outline" className="gap-1" title={names}>
        <Lock className="h-3 w-3" />
        {approvers.length} approver{approvers.length === 1 ? "" : "s"}
      </Badge>
      {rule.allow_self_approval === false && (
        <Badge variant="secondary" className="text-[10px]">
          four-eyes
        </Badge>
      )}
    </div>
  );
}

function RuleRow({
  rule,
  labeler,
}: {
  rule: PermissionRule;
  labeler: (scope: PermissionScope, id: string) => ScopeLabel;
}) {
  const subj = labeler(rule.scope, rule.scope_id);
  return (
    <tr className="border-b last:border-0">
      <td className="px-3 py-2 font-mono text-xs">{rule.tool_pattern}</td>
      <td className="px-3 py-2">
        <div className="flex flex-col gap-1">
          <StateBadge state={rule.state} />
          <ApproversBadge rule={rule} labeler={labeler} />
        </div>
      </td>
      <td className="px-3 py-2">
        <div className="flex items-center gap-1.5">
          <Badge variant="outline" className="text-[10px] uppercase">
            {rule.scope}
          </Badge>
          <span className="text-xs" title={rule.scope_id}>
            {subj.label}
          </span>
        </div>
      </td>
      <td className="px-3 py-2 text-xs text-muted-foreground">{rule.origin}</td>
      <td className="px-3 py-2 text-xs text-muted-foreground">
        {new Date(rule.updated_at).toLocaleString()}
      </td>
      <td className="px-3 py-2 text-right">
        <div className="inline-flex gap-1">
          <EditRuleDialog rule={rule} subjectLabel={subj.label} />
          <DeleteRuleButton rule={rule} subjectLabel={subj.label} />
        </div>
      </td>
    </tr>
  );
}

function RulesTable({
  rules,
  labeler,
}: {
  rules: PermissionRule[];
  labeler: (scope: PermissionScope, id: string) => ScopeLabel;
}) {
  if (rules.length === 0) {
    return (
      <p className="py-6 text-center text-sm text-muted-foreground">
        No rules match these filters.
      </p>
    );
  }
  return (
    <table className="w-full text-sm">
      <thead className="text-xs uppercase text-muted-foreground">
        <tr className="border-b">
          <th className="px-3 py-2 text-left">Tool pattern</th>
          <th className="px-3 py-2 text-left">State</th>
          <th className="px-3 py-2 text-left">Applies to</th>
          <th className="px-3 py-2 text-left">Origin</th>
          <th className="px-3 py-2 text-left">Updated</th>
          <th className="px-3 py-2 text-right">Actions</th>
        </tr>
      </thead>
      <tbody>
        {rules.map((rule) => (
          <RuleRow key={rule.id} rule={rule} labeler={labeler} />
        ))}
      </tbody>
    </table>
  );
}

function RulesTab() {
  const navigate = useNavigate();
  const [scope, setScope] = useState<PermissionScope | "">("");
  const [scopeId, setScopeId] = useState("");
  const [toolSearch, setToolSearch] = useState("");
  const debouncedToolSearch = useDebounced(toolSearch, 250);
  const labeler = useScopeLabeler();
  // The debounced value drives the server-side substring filter; the live value
  // drives an instant client-side narrow over what's already loaded.
  const filter: RuleFilter = useMemo(
    () => ({
      scope_type: scope || undefined,
      scope_id: scopeId.trim() || undefined,
      tool_pattern: debouncedToolSearch.trim() || undefined,
    }),
    [scope, scopeId, debouncedToolSearch],
  );

  const query = usePermissionRules(filter);
  const rules = query.data?.rules ?? [];
  const clientFiltered = useMemo(() => {
    const q = toolSearch.trim().toLowerCase();
    if (!q) return rules;
    return rules.filter((r) => r.tool_pattern.toLowerCase().includes(q));
  }, [rules, toolSearch]);

  return (
    <div className="space-y-4">
      <div className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/5 p-2 text-xs text-muted-foreground">
        <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-500" />
        <span>
          Rules are an <strong>optional fine-grained overlay</strong>. They only
          take effect when the rules layer is enabled (
          <span className="font-mono">[permissions] enabled</span> in
          workforce.toml). The always-on control is{" "}
          <strong>Roles &amp; Capabilities</strong>.
        </span>
      </div>
      <div className="flex items-end justify-between gap-3">
        <div className="grid flex-1 grid-cols-1 gap-3 md:grid-cols-3">
          <div>
            <label
              htmlFor="rules-scope"
              className="text-xs font-medium text-muted-foreground"
            >
              Scope
            </label>
            <select
              id="rules-scope"
              value={scope}
              onChange={(e) => {
                setScope(e.target.value as PermissionScope | "");
                setScopeId("");
              }}
              className="w-full rounded-md border bg-background px-2 py-1.5 text-sm"
            >
              <option value="">Any</option>
              <option value="team">Team</option>
              <option value="agent">Agent</option>
              <option value="user">Person</option>
            </select>
          </div>
          <div>
            <label
              htmlFor="rules-scope-id"
              className="text-xs font-medium text-muted-foreground"
            >
              Applies to
            </label>
            {scope === "" ? (
              <Input
                id="rules-scope-id"
                placeholder="Pick a scope to filter by name"
                value={scopeId}
                onChange={(e) => setScopeId(e.target.value)}
              />
            ) : (
              <EntityCombobox
                kind={scope}
                value={scopeId}
                id="rules-scope-id"
                onChange={(id) => setScopeId(id)}
              />
            )}
          </div>
          <div>
            <label
              htmlFor="rules-tool"
              className="text-xs font-medium text-muted-foreground"
            >
              Search tool pattern
            </label>
            <Input
              id="rules-tool"
              type="search"
              placeholder="e.g. memory or vault.delete"
              value={toolSearch}
              onChange={(e) => setToolSearch(e.target.value)}
            />
          </div>
        </div>
        <Button size="sm" onClick={() => navigate("/permissions/rules/new")}>
          <Plus className="mr-1.5 h-4 w-4" />
          New rule
        </Button>
      </div>

      {query.isLoading ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> Loading rules…
        </div>
      ) : query.isError ? (
        <CascadeDisabledNotice error={query.error as Error} />
      ) : (
        <Card>
          <CardContent className="p-0">
            <RulesTable rules={clientFiltered} labeler={labeler} />
          </CardContent>
        </Card>
      )}
    </div>
  );
}

function EffectiveTab() {
  const [userId, setUserId] = useState("");
  const [agentId, setAgentId] = useState("");
  const [toolName, setToolName] = useState("");
  const [teamIds, setTeamIds] = useState<string[]>([]);
  const [submitted, setSubmitted] = useState<EffectiveQuery | null>(null);

  const query = useEffectivePermission(submitted);

  const onLookup = () => {
    if (!userId || !agentId || !toolName) return;
    setSubmitted({
      user_id: userId,
      agent_id: agentId,
      tool_name: toolName,
      team_ids: teamIds,
    });
  };

  const result = query.data;

  return (
    <div className="space-y-4">
      <p className="text-xs text-muted-foreground">
        Resolve the effective permission for a (person, agent, tool) combination.
        The <strong>always-on capability matrix</strong> decides first; the{" "}
        <strong>optional rules overlay</strong> only changes the outcome when the
        rules layer is enabled.
      </p>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-4">
        <div>
          <label
            htmlFor="effective-user"
            className="text-xs font-medium text-muted-foreground"
          >
            Person
          </label>
          <EntityCombobox
            kind="user"
            value={userId}
            id="effective-user"
            onChange={(id) => setUserId(id)}
          />
        </div>
        <div>
          <label
            htmlFor="effective-agent"
            className="text-xs font-medium text-muted-foreground"
          >
            Agent
          </label>
          <EntityCombobox
            kind="agent"
            value={agentId}
            id="effective-agent"
            onChange={(id) => setAgentId(id)}
          />
        </div>
        <div>
          <label
            htmlFor="effective-tool"
            className="text-xs font-medium text-muted-foreground"
          >
            Tool
          </label>
          <ToolCombobox value={toolName} id="effective-tool" onChange={setToolName} />
        </div>
        <div>
          <label
            htmlFor="effective-teams"
            className="text-xs font-medium text-muted-foreground"
          >
            Teams (optional)
          </label>
          <EntityMultiCombobox
            kind="team"
            values={teamIds}
            id="effective-teams"
            onChange={setTeamIds}
          />
        </div>
      </div>
      <Button onClick={onLookup} disabled={!userId || !agentId || !toolName}>
        Evaluate
      </Button>

      {query.isLoading && submitted ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> Resolving…
        </div>
      ) : query.isError ? (
        <LoadErrorNotice error={query.error as Error} />
      ) : result ? (
        <Card>
          <CardContent className="space-y-4 p-4">
            <div className="flex flex-wrap items-center gap-3">
              <span className="text-sm font-medium">Effective:</span>
              <StateBadge state={result.effective} />
              <Badge variant="outline" className="text-[10px] uppercase">
                decided by{" "}
                {result.deciding_layer === "capability_matrix"
                  ? "capability matrix"
                  : result.deciding_layer === "rules"
                    ? "rules overlay"
                    : "no gate"}
              </Badge>
              <span className="ml-auto font-mono text-xs text-muted-foreground">
                {result.tool}
              </span>
            </div>

            <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
              <Card>
                <CardContent className="space-y-2 p-3">
                  <div className="flex items-center justify-between">
                    <span className="text-xs uppercase text-muted-foreground">
                      Capability matrix · always on
                    </span>
                    <AuditDecisionBadge decision={result.matrix_decision} />
                  </div>
                  <p className="text-xs text-muted-foreground">
                    Role <strong>{roleLabelFallback(result.user_role)}</strong>
                    {result.capability ? (
                      <>
                        {" "}
                        · governing capability{" "}
                        <span className="font-medium text-foreground">
                          {result.capability_label ?? result.capability}
                        </span>{" "}
                        <span className="font-mono">{result.capability}</span>
                      </>
                    ) : (
                      <> · this tool is not governed by a capability (ungated)</>
                    )}
                  </p>
                </CardContent>
              </Card>

              <Card>
                <CardContent className="space-y-2 p-3">
                  <div className="flex items-center justify-between">
                    <span className="text-xs uppercase text-muted-foreground">
                      Rules overlay · optional
                    </span>
                    {result.rules_enabled ? (
                      <StateBadge state={result.rules_decision} />
                    ) : (
                      <Badge variant="outline" className="text-[10px]">
                        disabled
                      </Badge>
                    )}
                  </div>
                  <p className="text-xs text-muted-foreground">
                    {result.rules_enabled
                      ? result.rules_decision
                        ? "A rule matched and overrode the matrix decision."
                        : "Enabled, but no rule matched this tool."
                      : "The rules layer is off, so it has no effect."}
                  </p>
                </CardContent>
              </Card>
            </div>

            {result.arg_dependent && result.branches && (
              <div className="space-y-2">
                <p className="text-xs font-medium text-muted-foreground">
                  This tool is argument-dependent — the capability (and decision)
                  depends on how it is called:
                </p>
                <Card>
                  <CardContent className="p-0">
                    <table className="w-full text-sm">
                      <thead className="text-xs uppercase text-muted-foreground">
                        <tr className="border-b">
                          <th className="px-3 py-2 text-left">When</th>
                          <th className="px-3 py-2 text-left">Capability</th>
                          <th className="px-3 py-2 text-left">Decision</th>
                        </tr>
                      </thead>
                      <tbody>
                        {result.branches.map((b, i) => (
                          <tr
                            key={`${b.when}-${i}`}
                            className="border-b last:border-0"
                          >
                            <td className="px-3 py-2 text-xs">{b.when}</td>
                            <td className="px-3 py-2 font-mono text-xs text-muted-foreground">
                              {b.capability ?? "—"}
                            </td>
                            <td className="px-3 py-2">
                              <StateBadge state={b.decision} />
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </CardContent>
                </Card>
              </div>
            )}
          </CardContent>
        </Card>
      ) : null}
    </div>
  );
}

/** Create a custom role, or edit an existing one (`role` set → edit, key is
 * immutable). The trigger is supplied by the caller (toolbar button vs. column
 * pencil). */
function RoleDialog({ role, trigger }: { role?: RoleRow; trigger: ReactNode }) {
  const editing = role !== undefined;
  const [open, setOpen] = useState(false);
  const [key, setKey] = useState(role?.key ?? "");
  const [name, setName] = useState(role?.label ?? "");
  const [description, setDescription] = useState(role?.description ?? "");
  const [error, setError] = useState("");
  const create = useCreateCustomRole();
  const update = useUpdateCustomRole();
  const pending = create.isPending || update.isPending;

  const reset = () => {
    setKey(role?.key ?? "");
    setName(role?.label ?? "");
    setDescription(role?.description ?? "");
    setError("");
  };

  const valid = (editing || key.trim().length > 0) && name.trim().length > 0;

  const onSubmit = () => {
    setError("");
    if (!valid) {
      setError(editing ? "Name is required." : "Key and name are required.");
      return;
    }
    const onSuccess = () => {
      setOpen(false);
      reset();
    };
    const onError = (e: Error) => setError(e.message);
    if (editing) {
      update.mutate(
        {
          id: role.key,
          body: { name: name.trim(), description: description.trim() },
        },
        { onSuccess, onError },
      );
    } else {
      create.mutate(
        {
          key: key.trim(),
          name: name.trim(),
          description: description.trim() || undefined,
        },
        { onSuccess, onError },
      );
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (!o) reset();
      }}
    >
      <DialogTrigger asChild>{trigger}</DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{editing ? "Edit role" : "New custom role"}</DialogTitle>
        </DialogHeader>
        <p className="text-xs text-muted-foreground">
          {editing
            ? "Rename or re-describe this role. Its key is fixed; edit capabilities in the matrix."
            : "A custom role adds a column to the matrix. New roles start from the member baseline; tune each capability below after creating."}
        </p>
        <div className="space-y-3">
          <div>
            <label htmlFor="role-key" className="text-xs font-medium">
              Key
            </label>
            <Input
              id="role-key"
              value={key}
              onChange={(e) => setKey(e.target.value)}
              placeholder="lowercase slug, e.g. agent_operator"
              disabled={editing}
            />
            {!editing && (
              <p className="mt-1 text-xs text-muted-foreground">
                Lowercase letters, digits and underscores; starts with a letter.
                This is the stable value stored on users.
              </p>
            )}
          </div>
          <div>
            <label htmlFor="role-name" className="text-xs font-medium">
              Name
            </label>
            <Input
              id="role-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. Agent Operator"
            />
          </div>
          <div>
            <label htmlFor="role-desc" className="text-xs font-medium">
              Description (optional)
            </label>
            <Input
              id="role-desc"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </div>
        </div>
        {error && <p className="text-sm text-destructive">{error}</p>}
        <div className="flex justify-end gap-2 pt-2">
          <Button variant="outline" onClick={() => setOpen(false)}>
            Cancel
          </Button>
          <Button onClick={onSubmit} disabled={pending || !valid}>
            {pending ? <Loader2 className="mr-1.5 h-4 w-4 animate-spin" /> : null}
            {editing ? "Save" : "Create"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function cellKey(role: string, capability: string): string {
  return `${role}:${capability}`;
}

function RolesAndCapabilitiesTab() {
  const matrix = useCapabilityMatrix();
  const catalog = useCapabilityCatalog();
  const update = useUpdateCapabilityMatrix();
  const deleteRole = useDeleteCustomRole();

  // Columns are roles (built-ins + customs) in API order; rows are capabilities
  // in the stable order the API returns (taken from the first role). Cells are
  // looked up by `role.key` + capability key — entirely API-driven.
  const roles = matrix.data?.roles ?? [];
  const protectedCaps = useMemo(
    () => new Set(matrix.data?.protected_capabilities ?? []),
    [matrix.data],
  );
  const capabilityOrder = roles[0]?.capabilities.map((c) => c.capability) ?? [];

  const roleLabelByKey = useMemo(() => {
    const m = new Map<string, string>();
    for (const r of roles) m.set(r.key, r.label);
    return m;
  }, [roles]);
  const roleLabel = (key: string): string =>
    roleLabelByKey.get(key) ?? roleLabelFallback(key);

  const catalogByKey = useMemo(() => {
    const m = new Map<string, CapabilityCatalogEntry>();
    for (const c of catalog.data?.capabilities ?? []) m.set(c.key, c);
    return m;
  }, [catalog.data]);
  const capabilityLabel = (key: string): string =>
    catalogByKey.get(key)?.label ?? humanizeKey(key);

  const decisionLookup = useMemo(() => {
    const byRole = new Map<string, Map<string, CapabilityDecisionValue>>();
    for (const role of roles) {
      byRole.set(
        role.key,
        new Map(role.capabilities.map((c) => [c.capability, c.decision])),
      );
    }
    return byRole;
  }, [roles]);

  const systemDefaults = matrix.data?.system_defaults ?? [];

  // Edited cells, keyed `role:capability`. A cell is dirty only when its edited
  // value differs from the server value, so reverting a select clears it.
  const [edits, setEdits] = useState<Map<string, CapabilityDecisionValue>>(
    new Map(),
  );
  const [reviewOpen, setReviewOpen] = useState(false);

  const original = (
    role: string,
    cap: string,
  ): CapabilityDecisionValue | undefined => decisionLookup.get(role)?.get(cap);

  const effective = (
    role: string,
    cap: string,
  ): CapabilityDecisionValue | undefined =>
    edits.get(cellKey(role, cap)) ?? original(role, cap);

  const isDirty = (role: string, cap: string): boolean => {
    const orig = original(role, cap);
    return orig !== undefined && effective(role, cap) !== orig;
  };

  const setCell = (role: string, cap: string, next: CapabilityDecisionValue) => {
    const orig = original(role, cap);
    setEdits((prev) => {
      const m = new Map(prev);
      const key = cellKey(role, cap);
      if (orig === undefined || next === orig) m.delete(key);
      else m.set(key, next);
      return m;
    });
  };

  const resetCell = (role: string, cap: string) => {
    setEdits((prev) => {
      const m = new Map(prev);
      m.delete(cellKey(role, cap));
      return m;
    });
  };

  // The structured diff every downstream view derives from: only changed cells.
  const changes = useMemo<CapabilityChangeView[]>(() => {
    const out: CapabilityChangeView[] = [];
    for (const [key, next] of edits) {
      const sep = key.indexOf(":");
      const role = key.slice(0, sep);
      const cap = key.slice(sep + 1);
      const orig = decisionLookup.get(role)?.get(cap);
      if (orig !== undefined && next !== orig) {
        out.push({ role, capability: cap, original: orig, next });
      }
    }
    return out;
  }, [edits, decisionLookup]);

  const onDeleteRole = (role: RoleRow) => {
    if (
      window.confirm(
        `Delete the custom role "${role.label}"? Anyone holding it reverts to Member.`,
      )
    ) {
      deleteRole.mutate(role.key);
    }
  };

  return (
    <div className="space-y-4">
      <p className="text-xs text-muted-foreground">
        The capability matrix is the always-on authorization model: each role can{" "}
        <strong>allow</strong>, <strong>ask</strong> for approval, or{" "}
        <strong>deny</strong> every privileged capability. A role <em>is</em> its
        column here. Edit a cell to change a role&rsquo;s access; review the diff
        before saving. To put a role on a person, edit it on their{" "}
        <Link to="/users" className="underline">
          user page
        </Link>
        .
      </p>

      {matrix.isLoading ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> Loading capabilities…
        </div>
      ) : matrix.isError ? (
        <LoadErrorNotice error={matrix.error as Error} />
      ) : (
        <>
          <div className="space-y-2">
            <h2 className="text-sm font-semibold">System defaults</h2>
            {systemDefaults.length === 0 ? (
              <Card>
                <CardContent className="p-4 text-sm text-muted-foreground">
                  No global defaults active.
                </CardContent>
              </Card>
            ) : (
              <div className="grid grid-cols-1 gap-2 md:grid-cols-2">
                {systemDefaults.map((def) => (
                  <Card key={`${def.pattern}-${def.state}`}>
                    <CardContent className="flex flex-wrap items-center gap-2 p-3">
                      <span className="text-sm font-medium">
                        {def.description || def.pattern}
                      </span>
                      <Badge
                        variant="outline"
                        className="gap-1 font-mono text-xs"
                      >
                        <Lock className="h-3 w-3" />
                        {def.pattern} → {def.state}
                      </Badge>
                      <Badge variant="success" className="text-[10px]">
                        active
                      </Badge>
                    </CardContent>
                  </Card>
                ))}
              </div>
            )}
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2">
              <h2 className="text-sm font-semibold">Roles &amp; capabilities</h2>
              <div className="flex items-center gap-2">
                {changes.length > 0 && (
                  <span className="text-xs text-muted-foreground">
                    {changes.length} unsaved change
                    {changes.length === 1 ? "" : "s"}
                  </span>
                )}
                <RoleDialog
                  trigger={
                    <Button size="sm" variant="outline">
                      <Plus className="mr-1.5 h-4 w-4" /> New role
                    </Button>
                  }
                />
                <Button
                  size="sm"
                  disabled={changes.length === 0}
                  onClick={() => {
                    update.reset();
                    setReviewOpen(true);
                  }}
                >
                  Save changes
                </Button>
              </div>
            </div>
            {capabilityOrder.length === 0 ? (
              <Card>
                <CardContent className="p-0">
                  <p className="py-6 text-center text-sm text-muted-foreground">
                    No capabilities reported.
                  </p>
                </CardContent>
              </Card>
            ) : (
              <Card>
                <CardContent className="overflow-x-auto p-0">
                  <table className="w-full text-sm">
                    <thead className="text-xs uppercase text-muted-foreground">
                      <tr className="border-b">
                        <th className="px-3 py-2 text-left">Capability</th>
                        {roles.map((role) => (
                          <th key={role.key} className="px-3 py-2 text-left">
                            <div className="flex items-center gap-1.5">
                              <span>{role.label}</span>
                              {!role.is_builtin && (
                                <>
                                  <Badge
                                    variant="outline"
                                    className="text-[9px]"
                                  >
                                    custom
                                  </Badge>
                                  <RoleDialog
                                    role={role}
                                    trigger={
                                      <Button
                                        size="icon"
                                        variant="ghost"
                                        className="h-5 w-5"
                                        aria-label={`Edit role ${role.label}`}
                                      >
                                        <Pencil className="h-3 w-3" />
                                      </Button>
                                    }
                                  />
                                  <Button
                                    size="icon"
                                    variant="ghost"
                                    className="h-5 w-5"
                                    disabled={deleteRole.isPending}
                                    onClick={() => onDeleteRole(role)}
                                    aria-label={`Delete role ${role.label}`}
                                  >
                                    <Trash2 className="h-3 w-3 text-destructive" />
                                  </Button>
                                </>
                              )}
                            </div>
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {capabilityOrder.map((cap) => {
                        const locked = protectedCaps.has(cap);
                        const entry = catalogByKey.get(cap);
                        return (
                          <tr key={cap} className="border-b last:border-0">
                            <td className="px-3 py-2">
                              <div className="flex items-center gap-1.5">
                                <span>{capabilityLabel(cap)}</span>
                                <CapabilityInfo entry={entry} />
                              </div>
                              <div className="font-mono text-xs text-muted-foreground">
                                {cap}
                              </div>
                            </td>
                            {roles.map((role) => {
                              const value = effective(role.key, cap);
                              if (value === undefined) {
                                return (
                                  <td key={role.key} className="px-3 py-2">
                                    <span className="text-muted-foreground">
                                      —
                                    </span>
                                  </td>
                                );
                              }
                              if (locked) {
                                return (
                                  <td key={role.key} className="px-3 py-2">
                                    <span className="inline-flex items-center gap-1.5">
                                      <Lock
                                        className="h-3 w-3 text-muted-foreground"
                                        aria-label="System Admin only; not editable"
                                      />
                                      <DecisionBadge decision={value} />
                                    </span>
                                  </td>
                                );
                              }
                              const dirty = isDirty(role.key, cap);
                              return (
                                <td key={role.key} className="px-3 py-2">
                                  <div className="flex items-center gap-1.5">
                                    <select
                                      aria-label={`${role.label} · ${capabilityLabel(cap)}`}
                                      className={cn(
                                        "rounded-md border bg-background px-2 py-1.5 text-sm capitalize",
                                        dirty &&
                                          "border-amber-500 ring-1 ring-amber-500",
                                      )}
                                      value={value}
                                      onChange={(e) =>
                                        setCell(
                                          role.key,
                                          cap,
                                          e.target
                                            .value as CapabilityDecisionValue,
                                        )
                                      }
                                    >
                                      {DECISION_CHOICES.map((d) => (
                                        <option key={d} value={d}>
                                          {d}
                                        </option>
                                      ))}
                                    </select>
                                    {dirty && (
                                      <Button
                                        size="icon"
                                        variant="ghost"
                                        className="h-6 w-6"
                                        title="Reset to saved value"
                                        aria-label="Reset cell"
                                        onClick={() => resetCell(role.key, cap)}
                                      >
                                        <RotateCcw className="h-3.5 w-3.5" />
                                      </Button>
                                    )}
                                  </div>
                                </td>
                              );
                            })}
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </CardContent>
              </Card>
            )}
          </div>

          <CapabilityChangeHistory roleLabel={roleLabel} />
        </>
      )}

      <SaveReviewDialog
        open={reviewOpen}
        onOpenChange={setReviewOpen}
        changes={changes}
        roleLabel={roleLabel}
        capabilityLabel={capabilityLabel}
        pending={update.isPending}
        error={update.error ? (update.error as Error).message : null}
        onConfirm={() => {
          const body: CapabilityChange[] = changes.map((c) => ({
            role: c.role,
            capability: c.capability,
            decision: c.next,
          }));
          update.mutate(
            { changes: body },
            {
              onSuccess: () => {
                setEdits(new Map());
                setReviewOpen(false);
              },
            },
          );
        }}
      />
    </div>
  );
}

function SaveReviewDialog({
  open,
  onOpenChange,
  changes,
  roleLabel,
  capabilityLabel,
  pending,
  error,
  onConfirm,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  changes: CapabilityChangeView[];
  roleLabel: (key: string) => string;
  capabilityLabel: (key: string) => string;
  pending: boolean;
  error: string | null;
  onConfirm: () => void;
}) {
  const [phrase, setPhrase] = useState("");

  const escalations = changes.filter(isEscalation);
  const massRevocations = changes.filter(isMassRevocation);
  const lockouts = changes.filter(isLockout);
  const dangerous =
    escalations.length > 0 || massRevocations.length > 0 || lockouts.length > 0;
  const confirmReady = dangerous ? phrase.trim() === DANGER_PHRASE : true;

  // Reset the typed phrase whenever the dialog closes so a prior confirmation
  // can't carry over to the next set of changes. The dialog is parent-
  // controlled, so Radix's `onOpenChange` never fires on an external close —
  // an effect on `open` is the only reliable reset point.
  useEffect(() => {
    if (!open) setPhrase("");
  }, [open]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] max-w-lg overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Review capability changes</DialogTitle>
        </DialogHeader>
        <p className="text-xs text-muted-foreground">
          {changes.length} change{changes.length === 1 ? "" : "s"} will be
          written. Only modified cells are sent.
        </p>

        <ul className="space-y-2">
          {changes.map((c) => {
            const escalation = isEscalation(c);
            const revocation = isMassRevocation(c);
            const lockout = isLockout(c);
            return (
              <li
                key={cellKey(c.role, c.capability)}
                className="rounded-md border p-2 text-sm"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{roleLabel(c.role)}</span>
                  <span className="text-muted-foreground">·</span>
                  <span>{capabilityLabel(c.capability)}</span>
                  <span className="font-mono text-xs text-muted-foreground">
                    {c.capability}
                  </span>
                </div>
                <div className="mt-1 flex flex-wrap items-center gap-2">
                  <DecisionBadge decision={c.original} />
                  <span className="text-muted-foreground">→</span>
                  <DecisionBadge decision={c.next} />
                  {escalation && (
                    <Badge variant="destructive" className="gap-1 text-[10px]">
                      <AlertTriangle className="h-3 w-3" />
                      grants
                    </Badge>
                  )}
                  {revocation && (
                    <Badge variant="destructive" className="gap-1 text-[10px]">
                      <AlertTriangle className="h-3 w-3" />
                      removes from all members
                    </Badge>
                  )}
                  {lockout && (
                    <Badge variant="destructive" className="gap-1 text-[10px]">
                      <AlertTriangle className="h-3 w-3" />
                      locks out admins
                    </Badge>
                  )}
                </div>
              </li>
            );
          })}
        </ul>

        {dangerous && (
          <div className="space-y-2 rounded-md border border-destructive/50 bg-destructive/5 p-3">
            <div className="flex items-start gap-2 text-sm">
              <AlertTriangle className="mt-0.5 h-4 w-4 text-destructive" />
              <div className="space-y-1">
                {escalations.length > 0 && (
                  <p>
                    {escalations.length} change
                    {escalations.length === 1 ? "" : "s"} loosen a
                    high-blast-radius capability.
                  </p>
                )}
                {massRevocations.length > 0 && (
                  <p>
                    {massRevocations.length} change
                    {massRevocations.length === 1 ? "" : "s"} restrict a
                    capability for all members.
                  </p>
                )}
                {lockouts.length > 0 && (
                  <p>
                    {lockouts.length} change
                    {lockouts.length === 1 ? "" : "s"} remove an admin's own
                    permission/role management (recover via the CLI break-glass).
                  </p>
                )}
              </div>
            </div>
            <label
              htmlFor="capability-danger-confirm"
              className="text-xs font-medium text-muted-foreground"
            >
              Type{" "}
              <span className="font-mono text-foreground">{DANGER_PHRASE}</span>{" "}
              to enable saving.
            </label>
            <Input
              id="capability-danger-confirm"
              value={phrase}
              onChange={(e) => setPhrase(e.target.value)}
              placeholder={DANGER_PHRASE}
              autoComplete="off"
            />
          </div>
        )}

        {error && <p className="text-sm text-destructive">{error}</p>}

        <div className="flex justify-end gap-2 pt-2">
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            variant={dangerous ? "destructive" : "default"}
            disabled={!confirmReady || pending || changes.length === 0}
            onClick={onConfirm}
          >
            {pending ? (
              <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
            ) : null}
            Save changes
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function CapabilityChangeHistory({
  roleLabel,
}: {
  roleLabel: (key: string) => string;
}) {
  const [open, setOpen] = useState(false);
  const audit = useCapabilityAudit();
  const catalog = useCapabilityCatalog();
  const labeler = useScopeLabeler();
  const capabilityLabel = (key: string): string =>
    catalog.data?.capabilities.find((c) => c.key === key)?.label ??
    humanizeKey(key);
  // `list_audit` already returns newest-first (ORDER BY at DESC, id DESC).
  const entries = audit.data?.entries ?? [];

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="space-y-2">
      <CollapsibleTrigger>
        {open ? (
          <ChevronDown className="h-3.5 w-3.5" />
        ) : (
          <ChevronRight className="h-3.5 w-3.5" />
        )}
        Change history
      </CollapsibleTrigger>
      <CollapsibleContent>
        {audit.isLoading ? (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" /> Loading change history…
          </div>
        ) : audit.isError ? (
          <LoadErrorNotice error={audit.error as Error} />
        ) : entries.length === 0 ? (
          <Card>
            <CardContent className="p-0">
              <p className="py-6 text-center text-sm text-muted-foreground">
                No capability changes recorded yet.
              </p>
            </CardContent>
          </Card>
        ) : (
          <Card>
            <CardContent className="p-0">
              <table className="w-full text-sm">
                <thead className="text-xs uppercase text-muted-foreground">
                  <tr className="border-b">
                    <th className="px-3 py-2 text-left">When</th>
                    <th className="px-3 py-2 text-left">Actor</th>
                    <th className="px-3 py-2 text-left">Role</th>
                    <th className="px-3 py-2 text-left">Capability</th>
                    <th className="px-3 py-2 text-left">Change</th>
                  </tr>
                </thead>
                <tbody>
                  {entries.map((e) => (
                    <tr key={e.id} className="border-b last:border-0">
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {new Date(e.at).toLocaleString()}
                      </td>
                      <td
                        className="px-3 py-2 text-xs"
                        title={e.actor_id ?? "system / CLI"}
                      >
                        {e.actor_id ? labeler("user", e.actor_id).label : "System"}
                      </td>
                      <td className="px-3 py-2 text-xs">{roleLabel(e.role)}</td>
                      <td className="px-3 py-2">
                        <div className="text-xs">
                          {capabilityLabel(e.capability)}
                        </div>
                        <div className="font-mono text-xs text-muted-foreground">
                          {e.capability}
                        </div>
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex flex-wrap items-center gap-1.5">
                          <OptionalDecisionBadge decision={e.old_decision} />
                          <span className="text-muted-foreground">→</span>
                          <OptionalDecisionBadge decision={e.new_decision} />
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </CardContent>
          </Card>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}

function AuditTab() {
  const [userFilter, setUserFilter] = useState("");
  const [resourceFilter, setResourceFilter] = useState("");
  const [showAllOps, setShowAllOps] = useState(false);
  const labeler = useScopeLabeler();
  const filter = useMemo(
    () => ({
      user_id: userFilter.trim() || undefined,
      resource: resourceFilter.trim() || undefined,
      operations: showAllOps ? "*" : undefined,
      limit: 200,
    }),
    [userFilter, resourceFilter, showAllOps],
  );
  const audit = usePermissionAudit(filter);

  return (
    <div className="space-y-4">
      <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
        <div>
          <label
            htmlFor="audit-user"
            className="text-xs font-medium text-muted-foreground"
          >
            User id
          </label>
          <Input
            id="audit-user"
            value={userFilter}
            onChange={(e) => setUserFilter(e.target.value)}
            placeholder="UUID — exact match"
          />
        </div>
        <div>
          <label
            htmlFor="audit-resource"
            className="text-xs font-medium text-muted-foreground"
          >
            Resource contains
          </label>
          <Input
            id="audit-resource"
            value={resourceFilter}
            onChange={(e) => setResourceFilter(e.target.value)}
            placeholder="tool name or rule UUID substring"
          />
        </div>
        <div className="flex items-end">
          <label className="flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              checked={showAllOps}
              onChange={(e) => setShowAllOps(e.target.checked)}
            />
            Show all operations (default: cascade events only)
          </label>
        </div>
      </div>

      {audit.isLoading ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> Loading audit events…
        </div>
      ) : audit.isError ? (
        <CascadeDisabledNotice error={audit.error as Error} />
      ) : audit.data ? (
        <Card>
          <CardContent className="p-0">
            {audit.data.events.length === 0 ? (
              <p className="py-6 text-center text-sm text-muted-foreground">
                No matching events in the last {audit.data.buffer_size}{" "}
                buffered records.
              </p>
            ) : (
              <table className="w-full text-sm">
                <thead className="text-xs uppercase text-muted-foreground">
                  <tr className="border-b">
                    <th className="px-3 py-2 text-left">When</th>
                    <th className="px-3 py-2 text-left">Op</th>
                    <th className="px-3 py-2 text-left">Decision</th>
                    <th className="px-3 py-2 text-left">User</th>
                    <th className="px-3 py-2 text-left">Owner / state</th>
                    <th className="px-3 py-2 text-left">Resource</th>
                  </tr>
                </thead>
                <tbody>
                  {audit.data.events.map((e, i) => (
                    <tr
                      key={`${e.timestamp}-${i}`}
                      className="border-b last:border-0"
                    >
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {new Date(e.timestamp).toLocaleString()}
                      </td>
                      <td className="px-3 py-2 font-mono text-xs">
                        {e.operation}
                      </td>
                      <td className="px-3 py-2 text-xs">
                        <AuditDecisionBadge decision={e.decision} />
                      </td>
                      <td className="px-3 py-2 text-xs">
                        {e.user_id ? (
                          <span title={e.user_id}>
                            {labeler("user", e.user_id).label}
                          </span>
                        ) : (
                          <span className="text-muted-foreground">—</span>
                        )}
                      </td>
                      <td className="px-3 py-2 font-mono text-xs text-muted-foreground">
                        {e.owner_or_state ?? "—"}
                      </td>
                      <td className="px-3 py-2 font-mono text-xs">
                        {e.resource ?? "—"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </CardContent>
        </Card>
      ) : null}
      {audit.data && (
        <p className="text-xs text-muted-foreground">
          {audit.data.count} event(s) shown · buffer holds{" "}
          {audit.data.buffer_size} of last 5000 events. Auto-refreshes every
          10s.
        </p>
      )}
    </div>
  );
}

const TAB_VALUES = ["roles", "effective", "rules", "audit"] as const;
type TabValue = (typeof TAB_VALUES)[number];

export default function PermissionsPage() {
  const [search, setSearch] = useSearchParams();
  const raw = search.get("tab");
  const tab: TabValue = (TAB_VALUES as readonly string[]).includes(raw ?? "")
    ? (raw as TabValue)
    : "roles";

  const onTabChange = (v: string) => {
    const next = new URLSearchParams(search);
    if (v === "roles") next.delete("tab");
    else next.set("tab", v);
    setSearch(next, { replace: true });
  };

  return (
    <div className="space-y-4 p-4">
      <div className="flex items-center gap-2">
        <Shield className="h-5 w-5" />
        <h1 className="text-xl font-semibold">Permissions</h1>
      </div>
      <p className="text-sm text-muted-foreground">
        Authorization has two layers. <strong>Roles &amp; Capabilities</strong>{" "}
        is the always-on control: a role&rsquo;s capabilities decide what every
        person and their agents may do. <strong>Rules</strong> are an optional
        fine-grained overlay that is inert unless the rules layer is enabled.
      </p>
      <Tabs value={tab} onValueChange={onTabChange}>
        <TabsList>
          <TabsTrigger value="roles">Roles &amp; Capabilities</TabsTrigger>
          <TabsTrigger value="effective">Effective permission</TabsTrigger>
          <TabsTrigger value="rules">
            Rules
            <Badge variant="outline" className="ml-1.5 text-[9px]">
              advanced
            </Badge>
          </TabsTrigger>
          <TabsTrigger value="audit">Audit</TabsTrigger>
        </TabsList>
        <TabsContent value="roles" className="pt-4">
          <RolesAndCapabilitiesTab />
        </TabsContent>
        <TabsContent value="effective" className="pt-4">
          <EffectiveTab />
        </TabsContent>
        <TabsContent value="rules" className="pt-4">
          <RulesTab />
        </TabsContent>
        <TabsContent value="audit" className="pt-4">
          <AuditTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
