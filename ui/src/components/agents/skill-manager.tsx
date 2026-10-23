import { useState, useMemo } from "react";
import { Link } from "react-router";
import {
  CheckCircle2, XCircle, ExternalLink, Key, Loader2, Save, Eye, EyeOff,
  Plus, ChevronDown, ChevronRight,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useGlobalSkills } from "@/hooks/use-skills";
import { useAgentVaultKeys, useAgentVaultSet } from "@/hooks/use-agents";
import type { GlobalSkill, SkillCredential } from "@/lib/api/skills";
import { apiErrorMessage } from "@/lib/api-error";

interface SkillManagerProps {
  agentId: string;
}

function discoverWorkspaces(vaultKeys: Set<string>, prefix: string): string[] {
  // Mirror the separator logic in WorkspaceGroup / AddWorkspaceDialog: a
  // prefix ending in "." is dot-notation (e.g. "slack.<ws>.<key>"),
  // anything else is slash-separated (e.g. "skills/github/workspaces/<org>/<key>").
  const sep = prefix.endsWith(".") ? "." : "/";
  const names = new Set<string>();
  for (const key of vaultKeys) {
    if (key.startsWith(prefix)) {
      const rest = key.slice(prefix.length);
      const name = rest.split(sep)[0];
      if (name) names.add(name);
    }
  }
  return [...names].sort();
}

export function SkillManager({ agentId }: SkillManagerProps) {
  const { data: skillsData, isLoading: skillsLoading, error: skillsError } = useGlobalSkills();
  const { data: vaultData, isLoading: vaultLoading, error: vaultError } = useAgentVaultKeys(agentId);

  const isLoading = skillsLoading || vaultLoading;
  const error = skillsError || vaultError;
  const skills = skillsData?.skills ?? [];
  const vaultKeys = new Set(vaultData?.keys ?? []);

  const sorted = [...skills].sort((a, b) => {
    const aScore = a.required_credentials.length > 0 || (a.workspace_credentials?.length ?? 0) > 0 ? 0 : a.has_config ? 1 : 2;
    const bScore = b.required_credentials.length > 0 || (b.workspace_credentials?.length ?? 0) > 0 ? 0 : b.has_config ? 1 : 2;
    return aScore - bScore || a.name.localeCompare(b.name);
  });

  if (error) {
    return (
      <div className="text-center py-12 text-destructive text-sm">
        Failed to load: {error instanceof Error ? error.message : "Unknown error"}
      </div>
    );
  }

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (skills.length === 0) {
    return (
      <div className="text-center py-12 text-muted-foreground">
        <p>No skills available.</p>
        <Link to="/skills" className="text-primary hover:underline text-sm mt-2 inline-block">
          Manage skills
        </Link>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <p className="text-xs text-muted-foreground">
        Configure skill credentials for this agent. Skill definitions are managed in{" "}
        <Link to="/skills" className="text-primary hover:underline">Skills</Link>.
      </p>
      <div className="space-y-2">
        {sorted.map((skill) => (
          <SkillCard key={skill.dir_name} skill={skill} agentId={agentId} vaultKeys={vaultKeys} />
        ))}
      </div>
    </div>
  );
}

function SkillCard({ skill, agentId, vaultKeys }: { skill: GlobalSkill; agentId: string; vaultKeys: Set<string> }) {
  const hasWorkspaces = !!skill.workspace_vault_prefix && (skill.workspace_credentials?.length ?? 0) > 0;
  const hasSingleCreds = skill.required_credentials.length > 0;

  const workspaceNames = useMemo(
    () => hasWorkspaces ? discoverWorkspaces(vaultKeys, skill.workspace_vault_prefix!) : [],
    [hasWorkspaces, vaultKeys, skill.workspace_vault_prefix],
  );

  const singleConfigured = skill.required_credentials.filter((c) => vaultKeys.has(c.vault_path));
  const singleReady = hasSingleCreds && singleConfigured.length === skill.required_credentials.filter((c) => c.required).length;

  let statusText: React.ReactNode;
  if (hasWorkspaces && workspaceNames.length > 0) {
    const label = skill.name === "email" ? "account" : "workspace";
    statusText = (
      <span className="flex items-center gap-1 text-xs text-emerald-500">
        <CheckCircle2 className="h-3.5 w-3.5" />
        {workspaceNames.length} {label}{workspaceNames.length !== 1 ? "s" : ""} configured
      </span>
    );
  } else if (hasSingleCreds && singleReady) {
    statusText = (
      <span className="flex items-center gap-1 text-xs text-emerald-500">
        <CheckCircle2 className="h-3.5 w-3.5" />
        {singleConfigured.length}/{skill.required_credentials.length} credentials set
      </span>
    );
  } else if (hasSingleCreds || hasWorkspaces) {
    statusText = (
      <span className="flex items-center gap-1 text-xs text-amber-500">
        <XCircle className="h-3.5 w-3.5" />
        Not configured
      </span>
    );
  } else if (skill.has_config) {
    statusText = (
      <Link to={`/skills/${skill.dir_name}`} className="flex items-center gap-1 text-xs text-primary hover:underline">
        Configured globally <ExternalLink className="h-3 w-3" />
      </Link>
    );
  } else {
    statusText = (
      <span className="flex items-center gap-1 text-xs text-muted-foreground">
        <CheckCircle2 className="h-3.5 w-3.5 text-emerald-500" /> Ready
      </span>
    );
  }

  const showBody = hasSingleCreds || hasWorkspaces;

  return (
    <div className="rounded-lg border bg-card">
      <div className="flex items-center justify-between px-4 py-3">
        <div className="flex items-center gap-3">
          <span className="text-sm font-medium">{skill.name}</span>
          <Badge variant={skill.type === "builtin" ? "secondary" : "outline"} className="text-[10px]">
            {skill.type === "builtin" ? "Built-in" : "External"}
          </Badge>
        </div>
        {statusText}
      </div>

      {showBody && (
        <div className="border-t px-4 py-3 space-y-3">
          {hasSingleCreds && (
            <div className="space-y-2">
              {hasWorkspaces && (
                <p className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">Single mode</p>
              )}
              {skill.required_credentials.map((cred) => (
                <CredentialRow key={cred.vault_path} credential={cred} agentId={agentId} isSet={vaultKeys.has(cred.vault_path)} />
              ))}
            </div>
          )}

          {hasWorkspaces && (
            <WorkspaceSection
              skill={skill}
              agentId={agentId}
              vaultKeys={vaultKeys}
              workspaceNames={workspaceNames}
            />
          )}
        </div>
      )}
    </div>
  );
}

function WorkspaceSection({
  skill,
  agentId,
  vaultKeys,
  workspaceNames,
}: {
  skill: GlobalSkill;
  agentId: string;
  vaultKeys: Set<string>;
  workspaceNames: string[];
}) {
  const [addOpen, setAddOpen] = useState(false);
  const prefix = skill.workspace_vault_prefix!;
  const templates = skill.workspace_credentials ?? [];
  const label = skill.name === "email" ? "Account" : "Workspace";

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <p className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">{label}s</p>
        <Button variant="outline" size="sm" className="h-6 text-[10px]" onClick={() => setAddOpen(true)}>
          <Plus className="h-3 w-3 mr-1" /> Add {label}
        </Button>
      </div>

      {workspaceNames.length === 0 && (
        <p className="text-xs text-muted-foreground">No {label.toLowerCase()}s configured yet.</p>
      )}

      {workspaceNames.map((wsName) => (
        <WorkspaceGroup key={wsName} name={wsName} prefix={prefix} templates={templates} agentId={agentId} vaultKeys={vaultKeys} />
      ))}

      <AddWorkspaceDialog
        open={addOpen}
        onClose={() => setAddOpen(false)}
        label={label}
        prefix={prefix}
        templates={templates}
        agentId={agentId}
      />
    </div>
  );
}

function WorkspaceGroup({
  name,
  prefix,
  templates,
  agentId,
  vaultKeys,
}: {
  name: string;
  prefix: string;
  templates: GlobalSkill["workspace_credentials"] & object;
  agentId: string;
  vaultKeys: Set<string>;
}) {
  const [expanded, setExpanded] = useState(false);
  const sep = prefix.endsWith(".") ? "." : "/";
  const creds = templates.map((t) => ({
    vault_path: `${prefix}${name}${sep}${t.key}`,
    description: t.description,
    required: t.required,
  }));
  const configuredCount = creds.filter((c) => vaultKeys.has(c.vault_path)).length;

  return (
    <div className="rounded-md border">
      <button
        type="button"
        className="flex items-center justify-between w-full px-3 py-2 text-left hover:bg-muted/50 transition-colors"
        onClick={() => setExpanded(!expanded)}
      >
        <div className="flex items-center gap-2">
          {expanded ? <ChevronDown className="h-3 w-3" /> : <ChevronRight className="h-3 w-3" />}
          <span className="text-xs font-medium">{name}</span>
        </div>
        <span className="text-[10px] text-muted-foreground">{configuredCount}/{creds.length} keys set</span>
      </button>
      {expanded && (
        <div className="border-t px-3 py-2 space-y-2">
          {creds.map((cred) => (
            <CredentialRow key={cred.vault_path} credential={cred} agentId={agentId} isSet={vaultKeys.has(cred.vault_path)} />
          ))}
        </div>
      )}
    </div>
  );
}

function AddWorkspaceDialog({
  open,
  onClose,
  label,
  prefix,
  templates,
  agentId,
}: {
  open: boolean;
  onClose: () => void;
  label: string;
  prefix: string;
  templates: GlobalSkill["workspace_credentials"] & object;
  agentId: string;
}) {
  const setVault = useAgentVaultSet(agentId);
  const [wsName, setWsName] = useState("");
  const [values, setValues] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const sep = prefix.endsWith(".") ? "." : "/";
  const kebabName = wsName.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  const requiredTemplates = templates.filter((t) => t.required);
  const allRequiredFilled = requiredTemplates.every((t) => (values[t.key] ?? "").trim().length > 0);
  const canSave = kebabName.length > 0 && allRequiredFilled;

  const handleSave = async () => {
    if (!canSave) return;
    setSaving(true);
    setError(null);
    try {
      for (const t of templates) {
        const val = (values[t.key] ?? "").trim();
        if (!val) continue;
        const path = `${prefix}${kebabName}${sep}${t.key}`;
        await setVault.mutateAsync({ path, value: val });
      }
      setWsName("");
      setValues({});
      onClose();
    } catch (e) {
      setError(apiErrorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const handleClose = () => {
    setWsName("");
    setValues({});
    setError(null);
    onClose();
  };

  return (
    <Dialog open={open} onOpenChange={(o) => !o && handleClose()}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>Add {label}</DialogTitle>
          <DialogDescription>
            Enter a name and credentials for the new {label.toLowerCase()}.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-1">
            <label className="text-sm font-medium">{label} Name</label>
            <Input
              placeholder={label === "Account" ? "e.g. personal-gmail" : "e.g. my-org"}
              value={wsName}
              onChange={(e) => setWsName(e.target.value)}
            />
            {kebabName && kebabName !== wsName && (
              <p className="text-[10px] text-muted-foreground">Will be saved as: <code>{kebabName}</code></p>
            )}
          </div>
          {kebabName && templates.map((t) => (
            <div key={t.key} className="space-y-1">
              <div className="flex items-center gap-2">
                <label className="text-sm font-medium">{t.description}</label>
                {t.required && <Badge variant="destructive" className="text-[10px]">Required</Badge>}
              </div>
              <Input
                type="password"
                placeholder={`${prefix}${kebabName}${sep}${t.key}`}
                value={values[t.key] ?? ""}
                onChange={(e) => setValues({ ...values, [t.key]: e.target.value })}
                className="font-mono text-xs"
              />
              <p className="text-[10px] text-muted-foreground font-mono">{prefix}{kebabName}{sep}{t.key}</p>
            </div>
          ))}
          {error && <p className="text-xs text-destructive">{error}</p>}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={handleClose}>Cancel</Button>
          <Button disabled={!canSave || saving} onClick={handleSave}>
            {saving ? "Saving..." : `Add ${label}`}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function CredentialRow({ credential, agentId, isSet }: { credential: SkillCredential; agentId: string; isSet: boolean }) {
  const setVault = useAgentVaultSet(agentId);
  const [value, setValue] = useState("");
  const [showInput, setShowInput] = useState(false);
  const [showValue, setShowValue] = useState(false);

  const handleSave = () => {
    if (!value.trim()) return;
    setVault.mutate(
      { path: credential.vault_path, value: value.trim() },
      { onSuccess: () => { setValue(""); setShowInput(false); } },
    );
  };

  return (
    <div className="flex items-center gap-3">
      <div className="shrink-0">
        {isSet ? <CheckCircle2 className="h-3.5 w-3.5 text-emerald-500" /> : <XCircle className="h-3.5 w-3.5 text-muted-foreground" />}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <code className="text-xs bg-muted px-1.5 py-0.5 rounded font-mono truncate">{credential.vault_path}</code>
          <span className="text-xs text-muted-foreground truncate">{credential.description}</span>
          {credential.required && !isSet && <Badge variant="destructive" className="text-[10px] shrink-0">Required</Badge>}
        </div>
        {setVault.error && (
          <p className="text-[10px] text-destructive mt-1">{apiErrorMessage(setVault.error)}</p>
        )}
      </div>
      <div className="flex items-center gap-1.5 shrink-0">
        {!showInput ? (
          <Button variant="outline" size="sm" className="h-7 text-xs" onClick={() => setShowInput(true)}>
            <Key className="h-3 w-3 mr-1" /> {isSet ? "Update" : "Set"}
          </Button>
        ) : (
          <div className="flex items-center gap-1.5">
            <div className="relative">
              <Input
                type={showValue ? "text" : "password"}
                placeholder="Enter value..."
                value={value}
                onChange={(e) => setValue(e.target.value)}
                className="h-7 text-xs w-48 pr-7 font-mono"
                onKeyDown={(e) => e.key === "Enter" && handleSave()}
              />
              <button
                type="button"
                className="absolute right-1.5 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                onClick={() => setShowValue(!showValue)}
              >
                {showValue ? <EyeOff className="h-3 w-3" /> : <Eye className="h-3 w-3" />}
              </button>
            </div>
            <Button size="sm" className="h-7 text-xs" disabled={!value.trim() || setVault.isPending} onClick={handleSave}>
              <Save className="h-3 w-3 mr-1" /> {setVault.isPending ? "..." : "Save"}
            </Button>
            <Button variant="ghost" size="sm" className="h-7 text-xs" onClick={() => { setShowInput(false); setValue(""); setVault.reset(); }}>
              Cancel
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
