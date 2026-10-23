import { useState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import {
  EntityCombobox,
  EntityMultiCombobox,
  useScopeLabeler,
} from "@/components/permissions/entity-picker";
import { ToolPicker } from "@/components/permissions/tool-picker";
import type {
  ApproverRef,
  ApproverRole,
  PermissionScope,
  PermissionState,
} from "@/lib/api/permissions";

const SCOPES: PermissionScope[] = ["team", "agent", "user"];

const KIND_LABEL: Record<PermissionScope, string> = {
  user: "Person",
  agent: "Agent",
  team: "Team",
};

const STATE_CHOICES: { value: PermissionState; label: string; hint: string }[] = [
  {
    value: "deny",
    label: "Deny",
    hint: "Block it entirely — no agent can run it.",
  },
  {
    value: "always_ask",
    label: "Always ask",
    hint: "Require human approval every time.",
  },
  {
    value: "ask",
    label: "Ask",
    hint: "Prompt for approval; user may save “always allow”.",
  },
  {
    value: "allow",
    label: "Allow",
    hint: "Permit it without prompting.",
  },
];

const APPROVER_ROLES: { value: ApproverRole; label: string }[] = [
  { value: "system_admin", label: "System admin" },
  { value: "manager", label: "Manager" },
  { value: "member", label: "Member" },
];

const APPROVER_ROLE_LABEL: Record<ApproverRole, string> = {
  system_admin: "System admin",
  manager: "Manager",
  member: "Member",
};

/** Routing config only applies to states that require approval. */
export function stateRoutes(state: PermissionState): boolean {
  return state === "ask" || state === "always_ask";
}

/** Split a rule's stored approvers into the two UI selections it edits. */
export function splitApprovers(approvers: ApproverRef[] | undefined): {
  userIds: string[];
  roles: ApproverRole[];
} {
  const userIds = new Set<string>();
  const roles = new Set<ApproverRole>();
  for (const a of approvers ?? []) {
    if (a.kind === "user") userIds.add(a.id);
    else roles.add(a.role);
  }
  return { userIds: [...userIds], roles: [...roles] };
}

/** Merge the two UI selections back into the wire `ApproverRef[]`. */
export function mergeApprovers(
  userIds: string[],
  roles: ApproverRole[],
): ApproverRef[] {
  return [
    ...roles.map((role): ApproverRef => ({ kind: "role", role })),
    ...userIds.map((id): ApproverRef => ({ kind: "user", id })),
  ];
}

/**
 * Designated-approver editor shared by the new-rule page and the edit dialog.
 * Reuses the person `EntityMultiCombobox` for users plus toggle chips for the
 * fixed identity roles.
 */
function ApproversSection({
  userIds,
  roles,
  allowSelfApproval,
  onUserIdsChange,
  onRolesChange,
  onAllowSelfApprovalChange,
}: {
  userIds: string[];
  roles: ApproverRole[];
  allowSelfApproval: boolean;
  onUserIdsChange: (ids: string[]) => void;
  onRolesChange: (roles: ApproverRole[]) => void;
  onAllowSelfApprovalChange: (value: boolean) => void;
}) {
  const labeler = useScopeLabeler();
  const toggleRole = (role: ApproverRole) =>
    onRolesChange(
      roles.includes(role) ? roles.filter((r) => r !== role) : [...roles, role],
    );
  const selected = [
    ...roles.map((role) => ({
      key: `role:${role}`,
      label: APPROVER_ROLE_LABEL[role],
      remove: () => onRolesChange(roles.filter((r) => r !== role)),
    })),
    ...userIds.map((id) => ({
      key: `user:${id}`,
      label: labeler("user", id).label,
      remove: () => onUserIdsChange(userIds.filter((u) => u !== id)),
    })),
  ];

  return (
    <div className="space-y-3 rounded-md border border-dashed p-3">
      <div className="space-y-0.5">
        <p className="text-xs font-medium text-muted-foreground">
          Approvers (optional)
        </p>
        <p className="text-xs text-muted-foreground">
          Route approval to these people / roles whenever this tool needs it.
          Leave empty for the requester to self-serve.
        </p>
      </div>

      {selected.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {selected.map((s) => (
            <Badge key={s.key} variant="secondary" className="gap-1">
              {s.label}
              <button
                type="button"
                onClick={s.remove}
                aria-label={`Remove ${s.label}`}
                className="text-muted-foreground hover:text-foreground"
              >
                ×
              </button>
            </Badge>
          ))}
        </div>
      )}

      <div className="space-y-1.5">
        <span className="text-xs font-medium text-muted-foreground">People</span>
        <EntityMultiCombobox
          kind="user"
          values={userIds}
          surface="inline"
          onChange={onUserIdsChange}
        />
      </div>

      <div className="space-y-1.5">
        <span className="text-xs font-medium text-muted-foreground">Roles</span>
        <div className="flex flex-wrap gap-1.5">
          {APPROVER_ROLES.map((r) => {
            const active = roles.includes(r.value);
            return (
              <Button
                key={r.value}
                type="button"
                size="sm"
                variant={active ? "default" : "outline"}
                aria-pressed={active}
                onClick={() => toggleRole(r.value)}
              >
                {r.label}
              </Button>
            );
          })}
        </div>
      </div>

      <label className="flex items-start gap-2 text-sm">
        <input
          type="checkbox"
          checked={allowSelfApproval}
          onChange={(e) => onAllowSelfApprovalChange(e.target.checked)}
          className="mt-0.5 rounded border-input"
        />
        <span>
          Allow self-approval
          <span className="block text-xs text-muted-foreground">
            When off, the requester can&rsquo;t approve their own request
            (four-eyes).
          </span>
        </span>
      </label>
    </div>
  );
}

/** The values a {@link RuleForm} emits on submit. In `create` mode `patterns`
 * may hold many tool patterns (one rule each); in `edit` mode it holds exactly
 * one and the scope/subject is fixed. */
export interface RuleFormValues {
  scope: PermissionScope;
  scopeId: string;
  scopeLabel: string;
  patterns: string[];
  state: PermissionState;
  approvers: ApproverRef[];
  allowSelfApproval: boolean;
}

export interface RuleFormInitial {
  scope?: PermissionScope;
  scopeId?: string;
  scopeLabel?: string;
  patterns?: string[];
  state?: PermissionState;
  approverUserIds?: string[];
  approverRoles?: ApproverRole[];
  allowSelfApproval?: boolean;
}

/**
 * The body of a permission rule — who, which tools, how to treat them, and the
 * optional approval routing. Owns its own field state; the parent owns the
 * mutation and supplies `submitLabel` / `submitting` / `error`.
 *
 * In `create` mode the scope is editable and the tool widget is multi-select
 * (the page submits N rules). In `edit` mode the scope is locked to
 * `subjectLabel` and exactly one tool pattern is edited.
 */
export function RuleForm({
  mode,
  initial,
  subjectLabel,
  submitLabel,
  submitting,
  error,
  onSubmit,
  onCancel,
}: {
  mode: "create" | "edit";
  initial?: RuleFormInitial;
  /** Pre-resolved subject label, shown in `edit` mode where scope is locked. */
  subjectLabel?: string;
  submitLabel: string;
  submitting: boolean;
  error?: string | null;
  onSubmit: (values: RuleFormValues) => void;
  onCancel?: () => void;
}) {
  const [scope, setScope] = useState<PermissionScope>(initial?.scope ?? "user");
  const [scopeId, setScopeId] = useState(initial?.scopeId ?? "");
  const [scopeLabel, setScopeLabel] = useState(initial?.scopeLabel ?? "");
  const [patterns, setPatterns] = useState<string[]>(initial?.patterns ?? []);
  const [singlePattern, setSinglePattern] = useState(
    initial?.patterns?.[0] ?? "",
  );
  const [state, setState] = useState<PermissionState>(initial?.state ?? "deny");
  const [approverUserIds, setApproverUserIds] = useState<string[]>(
    initial?.approverUserIds ?? [],
  );
  const [approverRoles, setApproverRoles] = useState<ApproverRole[]>(
    initial?.approverRoles ?? [],
  );
  const [allowSelfApproval, setAllowSelfApproval] = useState(
    initial?.allowSelfApproval ?? true,
  );
  const [localError, setLocalError] = useState("");

  const lockScope = mode === "edit";
  const effectivePatterns = lockScope
    ? singlePattern.trim()
      ? [singlePattern.trim()]
      : []
    : patterns;
  const canSubmit =
    (lockScope || scopeId.length > 0) && effectivePatterns.length > 0;

  const handleSubmit = () => {
    setLocalError("");
    if (!lockScope && !scopeId) {
      setLocalError("Pick who the rule applies to.");
      return;
    }
    if (effectivePatterns.length === 0) {
      setLocalError(
        lockScope ? "Tool pattern is required." : "Pick at least one tool or namespace.",
      );
      return;
    }
    // Routing only applies to ask/always_ask; allow/deny rules submit no
    // approvers and keep self-approval at its harmless default.
    const routed = stateRoutes(state);
    onSubmit({
      scope,
      scopeId,
      scopeLabel,
      patterns: effectivePatterns,
      state,
      approvers: routed ? mergeApprovers(approverUserIds, approverRoles) : [],
      allowSelfApproval: routed ? allowSelfApproval : true,
    });
  };

  return (
    <div className="space-y-4">
      <div className="space-y-1.5">
        <label className="text-xs font-medium text-muted-foreground">
          Who does this apply to?
        </label>
        {lockScope ? (
          <p className="text-sm">
            <Badge variant="outline" className="mr-2 text-[10px] uppercase">
              {scope}
            </Badge>
            <span className="font-medium">{subjectLabel ?? scopeId}</span>{" "}
            <span className="font-mono text-xs text-muted-foreground">
              {scopeId}
            </span>
          </p>
        ) : (
          <>
            <div className="flex gap-1">
              {SCOPES.map((s) => (
                <Button
                  key={s}
                  type="button"
                  size="sm"
                  variant={scope === s ? "default" : "outline"}
                  onClick={() => {
                    setScope(s);
                    setScopeId("");
                    setScopeLabel("");
                  }}
                >
                  {KIND_LABEL[s]}
                </Button>
              ))}
            </div>
            <EntityCombobox
              kind={scope}
              value={scopeId}
              surface="inline"
              onChange={(id, label) => {
                setScopeId(id);
                setScopeLabel(label);
              }}
            />
          </>
        )}
      </div>

      <div className="space-y-1.5">
        <label className="text-xs font-medium text-muted-foreground">
          Which tool calls?
        </label>
        {lockScope ? (
          <Input
            value={singlePattern}
            onChange={(e) => setSinglePattern(e.target.value)}
            placeholder="e.g. memory.* or vault.delete"
          />
        ) : (
          <ToolPicker selected={patterns} onChange={setPatterns} />
        )}
      </div>

      <div className="space-y-1.5">
        <label className="text-xs font-medium text-muted-foreground">
          How should agents treat it?
        </label>
        <div className="grid grid-cols-2 gap-2">
          {STATE_CHOICES.map((c) => (
            <button
              key={c.value}
              type="button"
              onClick={() => setState(c.value)}
              className={cn(
                "rounded-md border p-2 text-left",
                state === c.value
                  ? "border-primary ring-1 ring-primary"
                  : "hover:bg-accent",
              )}
            >
              <div className="text-sm font-medium">{c.label}</div>
              <div className="text-xs text-muted-foreground">{c.hint}</div>
            </button>
          ))}
        </div>
        {scope === "user" && state !== "deny" && (
          <p className="text-xs text-muted-foreground">
            A person-scoped “{state}” may be rejected if a Team / Agent rule
            already restricts the tool. <span className="font-mono">Deny</span>{" "}
            always applies.
          </p>
        )}
      </div>

      {stateRoutes(state) && (
        <ApproversSection
          userIds={approverUserIds}
          roles={approverRoles}
          allowSelfApproval={allowSelfApproval}
          onUserIdsChange={setApproverUserIds}
          onRolesChange={setApproverRoles}
          onAllowSelfApprovalChange={setAllowSelfApproval}
        />
      )}

      {(localError || error) && (
        <p className="text-sm text-destructive">{localError || error}</p>
      )}

      <div className="flex items-center justify-between gap-2 pt-2">
        <span className="text-xs text-muted-foreground">
          {!lockScope && patterns.length > 0 && scopeLabel
            ? `${patterns.length} rule${patterns.length === 1 ? "" : "s"} for ${scopeLabel}`
            : ""}
        </span>
        <div className="flex gap-2">
          {onCancel && (
            <Button variant="outline" onClick={onCancel}>
              Cancel
            </Button>
          )}
          <Button onClick={handleSubmit} disabled={submitting || !canSubmit}>
            {submitting ? (
              <Loader2 className="mr-1.5 h-4 w-4 animate-spin" />
            ) : null}
            {submitLabel}
          </Button>
        </div>
      </div>
    </div>
  );
}
