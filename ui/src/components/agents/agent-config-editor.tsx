import { useState, useCallback, useEffect, useRef } from "react";
import { Save, ChevronDown, ChevronRight, Plus, X, History, RefreshCw } from "lucide-react";
import { parse, stringify } from "smol-toml";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { TomlEditor } from "@/components/workflows/toml-editor";
import {
  useAgentConfig,
  useSaveAgentConfig,
  useAgentConfigRevisions,
  useRestoreAgentConfigRevision,
  useConfigCheck,
} from "@/hooks/use-agents";
import { useAgentLogins } from "@/hooks/use-subscriptions";
import { agentsApi } from "@/lib/api/agents";
import { ApiError, type Tagged } from "@/lib/api-client";
import { apiErrorMessage } from "@/lib/api-error";
import { useAuthStore } from "@/stores/auth-store";
import {
  attachFindings,
  blocksSave,
  refusalFindings,
  unattachedFindings,
  type Finding,
} from "@/lib/subscriptions/findings";
import { RevisionHistoryDialog } from "@/components/revisions/revision-history-dialog";
import { LoginPolicyEditor } from "@/components/agents/login-policy-editor";

/* ------------------------------------------------------------------ */
/*  Deep-get / deep-set helpers                                        */
/* ------------------------------------------------------------------ */

function deepGet(obj: Record<string, unknown>, path: string[]): unknown {
  let cur: unknown = obj;
  for (const key of path) {
    if (cur == null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[key];
  }
  return cur;
}

function deepSet(obj: Record<string, unknown>, path: string[], value: unknown): Record<string, unknown> {
  const root = structuredClone(obj);
  let cur: Record<string, unknown> = root;
  for (let i = 0; i < path.length - 1; i++) {
    const key = path[i]!;
    if (cur[key] == null || typeof cur[key] !== "object") {
      cur[key] = {};
    }
    cur = cur[key] as Record<string, unknown>;
  }
  const lastKey = path[path.length - 1]!;
  if (value === "" || value === undefined) {
    delete cur[lastKey];
  } else {
    cur[lastKey] = value;
  }
  return root;
}

/* ------------------------------------------------------------------ */
/*  Reusable form primitives                                           */
/* ------------------------------------------------------------------ */

function Section({ title, defaultOpen = true, children }: { title: string; defaultOpen?: boolean; children: React.ReactNode }) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <Card>
      <CardHeader className="cursor-pointer select-none py-3 px-4" onClick={() => setOpen(!open)}>
        <div className="flex items-center gap-2">
          {open ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
          <CardTitle className="text-sm font-semibold">{title}</CardTitle>
        </div>
      </CardHeader>
      {open && <CardContent className="px-4 pb-4 pt-0 space-y-3">{children}</CardContent>}
    </Card>
  );
}

const ROUTE_OPTIONS_ID = "agent-route-options";

const FINDING_TONE: Record<Finding["severity"], string> = {
  refused: "text-destructive",
  warning: "text-warning",
};

function FindingList({ findings }: { findings: Finding[] }) {
  return (
    <ul role="status" className="space-y-0.5">
      {findings.map((finding) => (
        <li key={`${finding.severity}:${finding.spec}:${finding.message}`} className={`text-xs ${FINDING_TONE[finding.severity]}`}>
          {finding.severity === "refused" ? "Refused: " : "Warning: "}
          {finding.message}
        </li>
      ))}
    </ul>
  );
}

function FormField({ label, value, onChange, placeholder, type = "text", list, findings = [] }: {
  label: string; value: string; onChange: (v: string) => void; placeholder?: string; type?: string;
  list?: string; findings?: Finding[];
}) {
  return (
    <div className="space-y-1">
      <label className="text-xs font-medium text-muted-foreground">{label}</label>
      <Input type={type} value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} list={list} className="h-8 text-sm" />
      <FindingList findings={attachFindings(findings, value)} />
    </div>
  );
}

function ToggleField({ label, checked, onChange }: {
  label: string; checked: boolean; onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between">
      <label className="text-xs font-medium text-muted-foreground">{label}</label>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        className={`relative inline-flex h-5 w-9 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors ${checked ? "bg-primary" : "bg-muted"}`}
        onClick={() => onChange(!checked)}
      >
        <span className={`pointer-events-none inline-block h-4 w-4 transform rounded-full bg-background shadow-lg transition-transform ${checked ? "translate-x-4" : "translate-x-0"}`} />
      </button>
    </div>
  );
}

function ArrayEditor({ label, items, onChange, placeholder, list, findings = [] }: {
  label: string; items: string[]; onChange: (items: string[]) => void; placeholder?: string;
  list?: string; findings?: Finding[];
}) {
  const [draft, setDraft] = useState("");
  return (
    <div className="space-y-1.5">
      <label className="text-xs font-medium text-muted-foreground">{label}</label>
      <div className="flex flex-wrap gap-1">
        {items.map((item, i) => (
          <Badge key={i} variant="secondary" className="gap-1 text-xs">
            {item}
            <button type="button" className="ml-0.5 hover:text-destructive" onClick={() => onChange(items.filter((_, j) => j !== i))}>
              <X className="h-3 w-3" />
            </button>
          </Badge>
        ))}
      </div>
      <FindingList findings={items.flatMap((item) => attachFindings(findings, item))} />
      <div className="flex gap-1">
        <Input
          className="h-7 text-xs flex-1"
          list={list}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={placeholder}
          onKeyDown={(e) => {
            if (e.key === "Enter" && draft.trim()) {
              e.preventDefault();
              onChange([...items, draft.trim()]);
              setDraft("");
            }
          }}
        />
        <Button type="button" variant="outline" size="sm" className="h-7 px-2" disabled={!draft.trim()}
          onClick={() => { onChange([...items, draft.trim()]); setDraft(""); }}>
          <Plus className="h-3 w-3" />
        </Button>
      </div>
    </div>
  );
}

function KeyValueEditor({ label, entries, onChange, list, findings = [] }: {
  label: string; entries: Record<string, string>; onChange: (entries: Record<string, string>) => void;
  list?: string; findings?: Finding[];
}) {
  const [draftKey, setDraftKey] = useState("");
  const [draftVal, setDraftVal] = useState("");
  const items = Object.entries(entries);
  return (
    <div className="space-y-1.5">
      <label className="text-xs font-medium text-muted-foreground">{label}</label>
      {items.length > 0 && (
        <div className="space-y-1">
          {items.map(([k, v]) => (
            <div key={k} className="flex items-center gap-1 text-xs">
              <Badge variant="outline" className="font-mono text-xs">{k}</Badge>
              <span className="text-muted-foreground">=</span>
              <span className="text-xs font-mono truncate flex-1">{v}</span>
              <button type="button" className="hover:text-destructive" onClick={() => {
                const next = { ...entries };
                delete next[k];
                onChange(next);
              }}>
                <X className="h-3 w-3" />
              </button>
            </div>
          ))}
        </div>
      )}
      <FindingList findings={items.flatMap(([, v]) => attachFindings(findings, v))} />
      <div className="flex gap-1">
        <Input className="h-7 text-xs flex-1" value={draftKey} onChange={(e) => setDraftKey(e.target.value)} placeholder="key" />
        <Input className="h-7 text-xs flex-1" value={draftVal} onChange={(e) => setDraftVal(e.target.value)} placeholder="value" list={list} />
        <Button type="button" variant="outline" size="sm" className="h-7 px-2" disabled={!draftKey.trim() || !draftVal.trim()}
          onClick={() => { onChange({ ...entries, [draftKey.trim()]: draftVal.trim() }); setDraftKey(""); setDraftVal(""); }}>
          <Plus className="h-3 w-3" />
        </Button>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------ */
/*  Main component                                                     */
/* ------------------------------------------------------------------ */

interface AgentConfigEditorProps {
  agentId: string;
}

export function AgentConfigEditor({ agentId }: AgentConfigEditorProps) {
  const { data: config, isLoading, refetch } = useAgentConfig(agentId);
  const saveMutation = useSaveAgentConfig(agentId);
  const [showHistory, setShowHistory] = useState(false);
  // Only fetch revisions while the dialog is open.
  const revisionsQuery = useAgentConfigRevisions(agentId, showHistory);
  const restoreMutation = useRestoreAgentConfigRevision(agentId);
  const isSystemAdmin = useAuthStore((s) => (s.identity ? s.identity.role === "system_admin" : true));
  const { data: logins } = useAgentLogins(agentId);

  const [toml, setToml] = useState("");
  const [etag, setEtag] = useState<string | null>(null);
  const [parsed, setParsed] = useState<Record<string, unknown>>({});
  const [parseError, setParseError] = useState<string | null>(null);
  const [parseErrorLine, setParseErrorLine] = useState<number | null>(null);
  const [dirty, setDirty] = useState(false);
  const dirtyRef = useRef(false);
  const tomlRef = useRef("");
  useEffect(() => {
    tomlRef.current = toml;
  }, [toml]);
  const syncSource = useRef<"toml" | "form" | null>(null);

  const markDirty = useCallback((value: boolean) => {
    dirtyRef.current = value;
    setDirty(value);
  }, []);

  const load = useCallback((loaded: Tagged) => {
    setToml(loaded.body);
    setEtag(loaded.etag);
    try {
      setParsed(parse(loaded.body) as Record<string, unknown>);
      setParseError(null);
      setParseErrorLine(null);
    } catch {
      setParsed({});
    }
    markDirty(false);
  }, [markDirty]);

  // A refetch replaces the editor's content only while it holds no edits;
  // an edit saved over a newer file is refused by its stale ETag instead.
  const body = config?.body;
  const tag = config?.etag ?? null;
  useEffect(() => {
    if (body !== undefined && !dirtyRef.current) load({ body, etag: tag });
  }, [body, tag, load]);

  const check = useConfigCheck(agentId, toml, isSystemAdmin && body !== undefined && !parseError);
  const findings = check.data?.findings ?? [];
  const checkRefusal = check.error instanceof ApiError && check.error.status === 400 ? check.error : null;
  const saveError = saveMutation.error;
  const stale = saveError instanceof ApiError && saveError.status === 409;

  /* TOML editor -> form */
  const handleTomlChange = useCallback((val: string) => {
    setToml(val);
    markDirty(true);
    if (syncSource.current === "form") return;
    syncSource.current = "toml";
    try {
      const p = parse(val) as Record<string, unknown>;
      setParsed(p);
      setParseError(null);
      setParseErrorLine(null);
    } catch (err) {
      const msg = err instanceof Error ? err.message : "Invalid TOML";
      setParseError(msg);
      const lineMatch = msg.match(/line (\d+)/i);
      setParseErrorLine(lineMatch ? parseInt(lineMatch[1]!, 10) : null);
    }
    syncSource.current = null;
  }, [markDirty]);

  /* Form -> TOML editor */
  const handleField = useCallback((path: string[], value: unknown) => {
    if (syncSource.current === "toml") return;
    syncSource.current = "form";
    try {
      const next = deepSet(parsed, path, value);
      setParsed(next);
      setToml(stringify(next));
      setParseError(null);
      setParseErrorLine(null);
      markDirty(true);
    } catch { /* ignore */ }
    syncSource.current = null;
  }, [parsed, markDirty]);

  /* Convenience getters */
  const str = (path: string[]) => String(deepGet(parsed, path) ?? "");
  const num = (path: string[]) => {
    const v = deepGet(parsed, path);
    return v != null ? String(v) : "";
  };
  const bool = (path: string[]) => deepGet(parsed, path) === true;
  const arr = (path: string[]) => {
    const v = deepGet(parsed, path);
    return Array.isArray(v) ? v.map(String) : [];
  };
  const kvObj = (path: string[]) => {
    const v = deepGet(parsed, path);
    if (v && typeof v === "object" && !Array.isArray(v)) {
      const result: Record<string, string> = {};
      for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
        result[k] = String(val);
      }
      return result;
    }
    return {};
  };

  const setStr = (path: string[]) => (v: string) => handleField(path, v || undefined);
  const setNum = (path: string[]) => (v: string) => {
    const n = v === "" ? undefined : Number(v);
    handleField(path, n != null && !isNaN(n) ? n : undefined);
  };
  const setBool = (path: string[]) => (v: boolean) => handleField(path, v);
  const setArr = (path: string[]) => (items: string[]) => handleField(path, items.length > 0 ? items : undefined);
  const setKv = (path: string[]) => (entries: Record<string, string>) =>
    handleField(path, Object.keys(entries).length > 0 ? entries : undefined);

  const routeOptions = logins?.route_options ?? [];
  const formValues = [
    str(["llm", "provider"]),
    str(["llm", "default_model"]),
    ...arr(["llm", "failover"]),
    ...Object.values(kvObj(["llm", "model_preferences"])),
    str(["compaction", "compaction_model"]),
  ];
  const saveBlocked = check.pending || blocksSave(findings) || checkRefusal !== null;

  function handleSave() {
    if (!dirty) return;
    const sent = toml;
    saveMutation.mutate(
      { toml: sent, etag },
      {
        onSuccess: (saved) => {
          setEtag(saved.etag);
          if (tomlRef.current === sent) markDirty(false);
        },
      },
    );
  }

  async function handleReload() {
    saveMutation.reset();
    const { data } = await refetch();
    if (data) load(data);
  }

  if (isLoading) {
    return <div className="text-sm text-muted-foreground py-8 text-center">Loading configuration...</div>;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <span className="text-sm font-medium">Agent Configuration (config.toml)</span>
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={() => setShowHistory(true)}>
            <History className="h-3.5 w-3.5 mr-1" />
            History
          </Button>
          <Button size="sm" disabled={!dirty || !!parseError || saveBlocked || stale || saveMutation.isPending} onClick={handleSave}>
            <Save className="h-3.5 w-3.5 mr-1" />
            {saveMutation.isPending ? "Saving..." : "Save"}
          </Button>
        </div>
      </div>

      {stale ? (
        <div role="status" className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-sm">
          <span>config.toml changed since you opened it. Reload to edit the current file; your unsaved edits here will be lost.</span>
          <Button variant="outline" size="sm" onClick={handleReload}>
            <RefreshCw className="h-3.5 w-3.5 mr-1" />
            Reload
          </Button>
        </div>
      ) : (
        saveError && (
          <div role="status" className="space-y-1 rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2">
            <p className="text-sm text-destructive">Not saved: {apiErrorMessage(saveError)}</p>
            <FindingList
              findings={refusalFindings(saveError).filter((f) => f.message !== apiErrorMessage(saveError))}
            />
          </div>
        )
      )}
      {checkRefusal && (
        <p role="status" className="text-sm text-destructive">
          The agent would not load this config: {apiErrorMessage(checkRefusal)}
        </p>
      )}
      <FindingList findings={unattachedFindings(findings, formValues)} />

      <datalist id={ROUTE_OPTIONS_ID}>
        {routeOptions.map((option) => (
          <option key={option} value={option} />
        ))}
      </datalist>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-4" style={{ minHeight: 500 }}>
        {/* TOML editor */}
        <div className="border rounded-lg overflow-hidden">
          <TomlEditor value={toml} onChange={handleTomlChange} parseError={parseError} parseErrorLine={parseErrorLine} />
        </div>

        {/* Form */}
        <div className="overflow-auto space-y-3" style={{ maxHeight: 600 }}>
          {/* [llm] */}
          <Section title="[llm]">
            <FormField label="Provider" value={str(["llm", "provider"])} onChange={setStr(["llm", "provider"])} placeholder="e.g. anthropic, chatgpt, claude-code, codex" findings={findings} />
            <FormField label="Default Model" value={str(["llm", "default_model"])} onChange={setStr(["llm", "default_model"])} placeholder="e.g. claude-sonnet-5" list={ROUTE_OPTIONS_ID} findings={findings} />
            <FormField label="Max Tokens" value={num(["llm", "max_tokens"])} onChange={setNum(["llm", "max_tokens"])} placeholder="e.g. 8192" type="number" />
            <FormField label="Max Rounds" value={num(["llm", "max_rounds"])} onChange={setNum(["llm", "max_rounds"])} placeholder="e.g. 10" type="number" />
            <ArrayEditor label="Failover" items={arr(["llm", "failover"])} onChange={setArr(["llm", "failover"])} placeholder="Add model name" list={ROUTE_OPTIONS_ID} findings={findings} />
            <FormField label="Thinking Budget Tokens" value={num(["llm", "thinking_budget_tokens"])} onChange={setNum(["llm", "thinking_budget_tokens"])} placeholder="e.g. 4096" type="number" />

            {/* [llm.routing] */}
            <div className="border-t pt-3 mt-3">
              <label className="text-xs font-semibold text-muted-foreground">[llm.routing]</label>
              <div className="space-y-3 mt-2">
                <ToggleField label="Enable Adaptive Routing" checked={bool(["llm", "routing", "enable_adaptive_routing"])} onChange={setBool(["llm", "routing", "enable_adaptive_routing"])} />
                <FormField label="Investigation Switch Round" value={num(["llm", "routing", "investigation_switch_round"])} onChange={setNum(["llm", "routing", "investigation_switch_round"])} placeholder="e.g. 3" type="number" />
                <FormField label="Coding Switch Round" value={num(["llm", "routing", "coding_switch_round"])} onChange={setNum(["llm", "routing", "coding_switch_round"])} placeholder="e.g. 5" type="number" />
              </div>
            </div>

            {/* [llm.model_preferences] */}
            <div className="border-t pt-3 mt-3">
              <KeyValueEditor
                label="[llm.model_preferences] (task type -> model)"
                entries={kvObj(["llm", "model_preferences"])}
                onChange={setKv(["llm", "model_preferences"])}
                list={ROUTE_OPTIONS_ID}
                findings={findings}
              />
            </div>

            {/* [llm.logins] */}
            <div className="border-t pt-3 mt-3 space-y-2">
              <label className="text-xs font-semibold text-muted-foreground">[llm.logins]</label>
              <LoginPolicyEditor
                block={deepGet(parsed, ["llm", "logins"])}
                effective={logins?.policy}
                disabled={!isSystemAdmin}
                onChange={(policy) => handleField(["llm", "logins"], policy)}
              />
            </div>
          </Section>

          {/* [compaction] */}
          <Section title="[compaction]">
            <FormField label="Max Context Tokens" value={num(["compaction", "max_context_tokens"])} onChange={setNum(["compaction", "max_context_tokens"])} placeholder="e.g. 100000" type="number" />
            <FormField label="Compaction Threshold" value={num(["compaction", "compaction_threshold"])} onChange={setNum(["compaction", "compaction_threshold"])} placeholder="0.0 - 1.0" type="number" />
            <FormField label="Keep Recent Messages" value={num(["compaction", "keep_recent_messages"])} onChange={setNum(["compaction", "keep_recent_messages"])} placeholder="e.g. 10" type="number" />
            <ToggleField label="Enable Memory Flush" checked={bool(["compaction", "enable_memory_flush"])} onChange={setBool(["compaction", "enable_memory_flush"])} />
            <ToggleField label="Prune Tool Outputs" checked={bool(["compaction", "prune_tool_outputs"])} onChange={setBool(["compaction", "prune_tool_outputs"])} />
            <FormField label="Prune Tool Output Max Chars" value={num(["compaction", "prune_tool_output_max_chars"])} onChange={setNum(["compaction", "prune_tool_output_max_chars"])} placeholder="e.g. 5000" type="number" />
            <FormField label="Compaction Model" value={str(["compaction", "compaction_model"])} onChange={setStr(["compaction", "compaction_model"])} placeholder="e.g. claude-haiku" list={ROUTE_OPTIONS_ID} findings={findings} />
            <FormField label="Urgent Compaction Threshold" value={num(["compaction", "urgent_compaction_threshold"])} onChange={setNum(["compaction", "urgent_compaction_threshold"])} placeholder="e.g. 0.95" type="number" />
          </Section>

          {/* [access] */}
          <Section title="[access]" defaultOpen={false}>
            <ToggleField label="Allow External Slack Users" checked={bool(["access", "allow_external_users"])} onChange={setBool(["access", "allow_external_users"])} />
            <FormField label="Deny Message" value={str(["access", "deny_message"])} onChange={setStr(["access", "deny_message"])} placeholder="Shown to denied users (optional)" />
          </Section>
        </div>
      </div>

      <RevisionHistoryDialog
        open={showHistory}
        onOpenChange={setShowHistory}
        title={`${agentId}/config.toml`}
        revisions={revisionsQuery.data?.revisions}
        isLoading={revisionsQuery.isLoading}
        currentContent={toml}
        getContent={(rev) =>
          agentsApi.getConfigRevision(agentId, rev).then((r) => r.content ?? "")
        }
        onRestore={(rev) =>
          restoreMutation.mutate(rev, {
            onSuccess: () => {
              markDirty(false);
              setShowHistory(false);
            },
            onError: (err) => alert(`Restore failed: ${apiErrorMessage(err)}`),
          })
        }
        restorePending={restoreMutation.isPending}
      />
    </div>
  );
}
