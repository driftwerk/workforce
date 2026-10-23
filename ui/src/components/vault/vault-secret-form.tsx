import { useEffect, useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
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
  useVaultCatalog,
  useVaultSet,
  useVaultRename,
  type CatalogEntry,
  type VaultEntryView,
} from "@/hooks/use-vault";
import { useAgents, useAgentVaultSet, useAgentVaultMove } from "@/hooks/use-agents";
import { useIdentities } from "@/hooks/use-identities";
import { useSetMySecret } from "@/hooks/use-my-settings";
import { useAuthStore } from "@/stores/auth-store";
import { apiErrorMessage } from "@/lib/api-error";
import {
  resolveTemplate,
  hasUnfilledVariables,
  matchTemplate,
  vaultEntryOwnerId,
} from "@/lib/api/vault";

const CUSTOM = "__custom__";

const selectClass =
  "flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50";

export type VaultContext =
  | { kind: "global" }
  | { kind: "agent"; agentId: string }
  // My Settings: a member adding to their own per-user namespace. Writes go
  // through /api/me/secret (not the admin vault), locked to the current user;
  // the only choice is which per-user tier (global vs a specific agent).
  | { kind: "member" };

interface VaultSecretFormProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  context: VaultContext;
  /** "add" creates a new secret (with a value); "rename" fixes an existing entry's path. */
  mode: "add" | "rename";
  /** Required for rename mode — the entry being repathed. */
  entry?: VaultEntryView | null;
}

/** Find the catalog entry whose template matches `path`, with extracted variables. */
function matchCatalog(
  path: string,
  catalog: CatalogEntry[],
): { entry: CatalogEntry; vars: Record<string, string> } | null {
  for (const entry of catalog) {
    const vars = matchTemplate(entry.template, path);
    if (vars) return { entry, vars };
  }
  return null;
}

export function VaultSecretForm({
  open,
  onOpenChange,
  context,
  mode,
  entry,
}: VaultSecretFormProps) {
  const isAgent = context.kind === "agent";
  const isMember = context.kind === "member";
  const agentId = context.kind === "agent" ? context.agentId : "";

  const { data: catalogData } = useVaultCatalog();
  const catalog = useMemo(() => catalogData?.entries ?? [], [catalogData]);
  const { data: identitiesData } = useIdentities();
  const { data: agentsData } = useAgents();
  const memberAgents = useMemo(() => agentsData?.agents ?? [], [agentsData]);
  const currentUser = useAuthStore((s) => s.identity);

  const vaultSet = useVaultSet();
  const vaultRename = useVaultRename();
  const agentSet = useAgentVaultSet(agentId);
  const agentMove = useAgentVaultMove(agentId);
  const setMySecret = useSetMySecret();

  const perUserTier = isAgent ? "agent_per_user" : "global_per_user";
  const sharedTier = isAgent ? "agent_specific" : "global";

  const [templateId, setTemplateId] = useState("");
  const [vars, setVars] = useState<Record<string, string>>({});
  const [customPath, setCustomPath] = useState("");
  const [scope, setScope] = useState<"shared" | "per_user">("shared");
  const [userId, setUserId] = useState("");
  // Member mode: "global" or "agent:<id>" — which per-user tier to write to.
  const [memberDest, setMemberDest] = useState("global");
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);

  // Initialise state when the dialog opens. Catalog is read for rename prefill
  // but kept out of the deps so a late catalog load can't wipe in-progress input.
  useEffect(() => {
    if (!open) return;
    setError(null);
    setValue("");
    if (mode === "rename" && entry) {
      const entryPerUser = entry.tier.endsWith("per_user");
      setScope(entryPerUser ? "per_user" : "shared");
      setUserId(entryPerUser ? vaultEntryOwnerId(entry) : "");
      // Runtime (non-addable) matches aren't in the dropdown, so fall back to a
      // custom-path edit rather than selecting an unlisted option.
      const matched = matchCatalog(entry.path, catalog);
      const usable = matched && matched.entry.addable !== false ? matched : null;
      setTemplateId(usable ? usable.entry.id : CUSTOM);
      setVars(usable ? usable.vars : {});
      setCustomPath(usable ? "" : entry.path);
    } else {
      setTemplateId("");
      setVars({});
      setCustomPath("");
      setScope("shared");
      setUserId(currentUser?.id ?? "");
      setMemberDest("global");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, mode, entry]);

  const selectedEntry = catalog.find((e) => e.id === templateId) ?? null;
  const isCustom = templateId === CUSTOM;

  // Scope gating from the selected catalog entry.
  const scopeLocked =
    mode === "rename" || (!!selectedEntry && selectedEntry.scope !== "either");
  useEffect(() => {
    if (mode === "rename") return;
    if (selectedEntry?.scope === "shared") setScope("shared");
    else if (selectedEntry?.scope === "per_user") setScope("per_user");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [templateId]);

  const logical = isCustom
    ? customPath.trim()
    : selectedEntry
      ? resolveTemplate(selectedEntry.template, vars)
      : "";

  const memberAgentId = memberDest.startsWith("agent:")
    ? memberDest.slice("agent:".length)
    : undefined;
  const memberTier = memberAgentId ? "agent_per_user" : "global_per_user";

  const physicalPreview =
    isMember && currentUser
      ? `users/${currentUser.id}/${logical}`
      : scope === "per_user" && userId
        ? `users/${userId}/${logical}`
        : logical;

  // Group offerable catalog entries for the dropdown, split by skill + origin so
  // a built-in skill and its MCP counterpart (e.g. hubspot) read distinctly.
  // Runtime OAuth material (addable === false) is never offered; a member sees
  // only what their per-user tiers may hold.
  const groups = useMemo(() => {
    const m = new Map<
      string,
      { group: string; source: "builtin" | "mcp"; items: CatalogEntry[] }
    >();
    for (const e of catalog) {
      if (e.addable === false || (isMember && !e.per_user_allowed)) continue;
      const source = e.source ?? "builtin";
      const key = `${e.group} ${source}`;
      const g = m.get(key) ?? { group: e.group, source, items: [] };
      g.items.push(e);
      m.set(key, g);
    }
    return [...m.values()].sort(
      (a, b) => a.group.localeCompare(b.group) || a.source.localeCompare(b.source),
    );
  }, [catalog, isMember]);

  const identities = identitiesData?.identities ?? [];
  const sortedIdentities = useMemo(() => {
    const list = [...identities];
    list.sort((a, b) => {
      if (a.id === currentUser?.id) return -1;
      if (b.id === currentUser?.id) return 1;
      return a.display_name.localeCompare(b.display_name);
    });
    return list;
  }, [identities, currentUser]);

  const pending =
    vaultSet.isPending ||
    vaultRename.isPending ||
    agentSet.isPending ||
    agentMove.isPending ||
    setMySecret.isPending;

  const incomplete =
    !logical ||
    hasUnfilledVariables(logical) ||
    (!isMember && scope === "per_user" && !userId) ||
    (mode === "add" && !value.trim()) ||
    (mode === "rename" && !!entry && logical === entry.path);

  function close() {
    onOpenChange(false);
  }

  function onErr(e: unknown) {
    setError(apiErrorMessage(e));
  }

  function handleSubmit() {
    setError(null);
    if (incomplete) return;

    if (mode === "add") {
      if (isMember) {
        setMySecret.mutate(
          {
            path: logical,
            value: value.trim(),
            tier: memberTier,
            agent_id: memberAgentId,
          },
          { onSuccess: close, onError: onErr },
        );
        return;
      }
      const physical = scope === "per_user" ? `users/${userId}/${logical}` : logical;
      if (isAgent) {
        agentSet.mutate(
          { path: physical, value: value.trim() },
          { onSuccess: close, onError: onErr },
        );
      } else {
        vaultSet.mutate(
          { path: physical, value: value.trim() },
          { onSuccess: close, onError: onErr },
        );
      }
      return;
    }

    // rename — keep the same tier/owner, change only the path.
    if (!entry) return;
    if (isAgent) {
      const user_id = entry.tier.endsWith("per_user")
        ? vaultEntryOwnerId(entry)
        : undefined;
      agentMove.mutate(
        {
          path: entry.path,
          from_tier: entry.tier,
          to_tier: entry.tier,
          to_path: logical,
          user_id,
        },
        { onSuccess: close, onError: onErr },
      );
    } else {
      const owner = vaultEntryOwnerId(entry);
      const oldPath =
        entry.tier === "global_per_user" ? `users/${owner}/${entry.path}` : entry.path;
      const newPath =
        entry.tier === "global_per_user" ? `users/${owner}/${logical}` : logical;
      vaultRename.mutate({ oldPath, newPath }, { onSuccess: close, onError: onErr });
    }
  }

  const title =
    mode === "add"
      ? isMember
        ? "Add secret"
        : `Add secret (${isAgent ? "agent" : "global"} vault)`
      : "Rename / fix path";

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {mode === "add"
              ? "Pick a known credential path or enter a custom one, choose where it's stored, then set the value."
              : "Move this secret to the correct path. The storage tier and owner are kept; only the path changes."}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          {/* Path template */}
          <div className="space-y-1.5">
            <label className="text-sm font-medium leading-none">Credential</label>
            <select
              className={selectClass}
              value={templateId}
              onChange={(e) => {
                setTemplateId(e.target.value);
                setVars({});
                setError(null);
              }}
            >
              <option value="">Select a credential…</option>
              {groups.map((g) => (
                <optgroup
                  key={`${g.group} ${g.source}`}
                  label={`${g.group} · ${g.source === "mcp" ? "MCP" : "built-in"}`}
                >
                  {g.items.map((e) => (
                    <option key={e.id} value={e.id}>
                      {e.label}
                      {e.managed_by === "oauth" ? " · OAuth" : ""}
                    </option>
                  ))}
                </optgroup>
              ))}
              <option value={CUSTOM}>Custom path…</option>
            </select>
            {selectedEntry && !selectedEntry.skill && selectedEntry.description && (
              <p className="text-xs text-muted-foreground">
                {selectedEntry.description}
              </p>
            )}
          </div>

          {/* Variable ("Change-Me") segments */}
          {selectedEntry?.variables?.map((v) => (
            <div key={v.name} className="space-y-1.5">
              <label className="text-sm font-medium leading-none">{v.label}</label>
              <Input
                placeholder={v.placeholder}
                value={vars[v.name] ?? ""}
                onChange={(e) => setVars((p) => ({ ...p, [v.name]: e.target.value }))}
              />
            </div>
          ))}

          {/* Custom free-text path */}
          {isCustom && (
            <div className="space-y-1.5">
              <label className="text-sm font-medium leading-none">Path</label>
              <Input
                placeholder="e.g. skills/foo/bar/api_key"
                value={customPath}
                onChange={(e) => setCustomPath(e.target.value)}
              />
            </div>
          )}

          {/* OAuth-managed warning */}
          {selectedEntry?.managed_by === "oauth" && mode === "add" && (
            <div className="flex gap-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-2 text-xs text-amber-700 dark:text-amber-400">
              <AlertTriangle className="h-4 w-4 shrink-0" />
              <span>
                This credential is normally written by an OAuth setup flow. A
                value set here may be overwritten on the next token refresh —
                prefer the skill's setup action unless you know what you're doing.
              </span>
            </div>
          )}

          {/* Resolved path preview */}
          {logical && (
            <div className="rounded-md bg-muted px-3 py-2 text-xs">
              <span className="text-muted-foreground">Stored at: </span>
              <span className="font-mono">{physicalPreview}</span>
            </div>
          )}

          {/* Scope + user (admin: shared vs per-user, on anyone's behalf) */}
          {!isMember && (
            <>
              <div className="space-y-1.5">
                <label className="text-sm font-medium leading-none">Scope</label>
                <select
                  className={selectClass}
                  value={scope}
                  disabled={scopeLocked}
                  onChange={(e) => setScope(e.target.value as "shared" | "per_user")}
                >
                  <option value="shared">
                    {isAgent ? "Agent-shared (all users of this agent)" : "Global (shared by everyone)"}
                  </option>
                  <option value="per_user">
                    {isAgent ? "Per-user (this agent)" : "Global · per-user"}
                  </option>
                </select>
                <p className="text-xs text-muted-foreground">
                  {scope === "per_user"
                    ? `Stored under users/<id>/… (tier ${perUserTier}).`
                    : `Stored at the bare path (tier ${sharedTier}).`}
                </p>
                {scope === "per_user" &&
                  selectedEntry &&
                  !selectedEntry.per_user_allowed && (
                    <p className="text-xs text-muted-foreground">
                      Note: users can't set this path themselves from Settings — only
                      an admin can write it here.
                    </p>
                  )}
              </div>

              {scope === "per_user" && (
                <div className="space-y-1.5">
                  <label className="text-sm font-medium leading-none">User</label>
                  <select
                    className={selectClass}
                    value={userId}
                    disabled={mode === "rename"}
                    onChange={(e) => setUserId(e.target.value)}
                  >
                    <option value="">Select a user…</option>
                    {sortedIdentities.map((u) => (
                      <option key={u.id} value={u.id}>
                        {u.display_name}
                        {u.id === currentUser?.id ? " (you)" : ""}
                      </option>
                    ))}
                  </select>
                </div>
              )}
            </>
          )}

          {/* Destination (member: which of your own per-user tiers) */}
          {isMember && (
            <div className="space-y-1.5">
              <label className="text-sm font-medium leading-none">Scope</label>
              <select
                className={selectClass}
                value={memberDest}
                onChange={(e) => setMemberDest(e.target.value)}
              >
                <option value="global">Global · per-user (all your agents)</option>
                {memberAgents.map((a) => (
                  <option key={a.id} value={`agent:${a.id}`}>
                    Agent · per-user — {a.name ?? a.id}
                  </option>
                ))}
              </select>
              <p className="text-xs text-muted-foreground">
                Stored privately under{" "}
                <span className="font-mono">users/&lt;you&gt;/…</span> (tier {memberTier}).
                Only you can read it.
              </p>
            </div>
          )}

          {/* Value (add only) */}
          {mode === "add" && (
            <div className="space-y-1.5">
              <label className="text-sm font-medium leading-none">Value</label>
              {selectedEntry?.multiline ? (
                <textarea
                  className="flex min-h-[120px] w-full resize-y rounded-md border border-input bg-background px-3 py-2 font-mono text-xs ring-offset-background placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
                  placeholder="Paste the whole file"
                  value={value}
                  onChange={(e) => setValue(e.target.value)}
                  spellCheck={false}
                />
              ) : (
                <Input
                  type="password"
                  placeholder="Secret value"
                  value={value}
                  onChange={(e) => setValue(e.target.value)}
                />
              )}
            </div>
          )}

          {error && <p className="text-xs text-destructive">{error}</p>}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={close}>
            Cancel
          </Button>
          <Button disabled={incomplete || pending} onClick={handleSubmit}>
            {pending ? "Saving…" : mode === "add" ? "Save" : "Rename"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
