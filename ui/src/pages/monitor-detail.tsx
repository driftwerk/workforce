import { useCallback, useEffect, useMemo, useState } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router";
import { parse as parseToml } from "smol-toml";
import { ArrowLeft, Save, Diff, Trash2, History, Play, Pause } from "lucide-react";
import { VisibilitySelect } from "@/components/sharing/visibility-select";
import {
  definitionVisibility,
  definitionWord,
  visibilityChoices,
} from "@/lib/access";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { PageSkeleton } from "@/components/loading-skeleton";
import {
  useMonitors,
  useMonitorSource,
  useSaveMonitor,
  useMonitorRevisions,
  useRestoreMonitorRevision,
} from "@/hooks/use-monitors";
import { monitorsApi } from "@/lib/api/monitors";
import { RevisionHistoryDialog } from "@/components/revisions/revision-history-dialog";
import { TomlEditor } from "@/components/workflows/toml-editor";
import { WorkflowDiff } from "@/components/workflows/workflow-diff";
import { DeleteMonitorDialog } from "@/components/monitors/delete-monitor-dialog";
import { RecentActivity } from "@/components/monitors/recent-activity";
import { MonitorStatusBadge, troubleHint } from "@/components/monitors/monitor-status";
import type { MonitorDef, MonitorKind } from "@/lib/api/monitors";

const NEW_CHANNEL_STUB = `[monitor]
id = "my-monitor"
name = "New Monitor"
description = ""
enabled = true
# agent = "lynx"         # optional: whose bot watches the channel (default: the action's
                         # agent, else the hub's default agent). Invite that bot to the channel.
# once = true            # optional: stop after the first successful fire
# expires_at = "2026-12-31T00:00:00Z"   # optional: stop then, telling the owner

[monitor.binding]
surface = "slack"        # "slack" | "teams" | "any"
channel = "C0XXXXXXX"    # the channel id to watch
# exclude_thread_replies = true   # optional (default false): only top-level
                                  # channel messages, never thread replies

[monitor.filter]
type = "contains_any"    # all | contains_any | regex | from_user | from_bot | from_human | has_file
values = ["released", "shipped"]
# pattern = "v\\\\d+"      # for type = "regex"
# user = "U0XXXXXXX"     # for type = "from_user"

# [monitor.decide]       # optional: one yes/no to the decision model before the action
# question = "Is this a release announcement rather than a question about one?"
# yes = "a version was shipped"    # optional criteria the model judges by
# no = "a question, a plan, or a bug report"
# min_probability = 0.7

[monitor.action]
type = "run_workflow"    # run_workflow | run_skill | notify | agent_task | persona
workflow = "my-workflow"
# --- or run a skill tool directly, no model turn ---
# type = "run_skill"
# tool = "my-skill.record"
# input = { text = "{{text}}", from = "{{user}}" }
# --- or notify a channel / DM a user (static template) ---
# type = "notify"
# target = "C0OUTPUT"            # channel id, or "@U0XXXXXXX" to DM a user
# template = "New release: {{text}}"   # {{text}} {{user}} {{channel}} {{ts}} {{json:/a/b}}
# --- or run a one-shot agent turn and deliver its reply ---
# type = "agent_task"
# agent = "lynx"                 # optional; defaults to the default agent
# prompt = "Summarise the message and tell the user what changed."
# deliver_to = "@U0XXXXXXX"      # #channel id or @user DM
# --- or make an agent converse in the channel (standing persona binding) ---
# type = "persona"
# agent = "lynx"                 # must be bound to the channel/workspace
# prompt = "You are the triage assistant for this channel. ..."
# mode = "all"                   # optional: all | mention_only | quiet (default all)
`;

const NEW_SOURCE_STUB = `[monitor]
id = "my-source-monitor"
name = "New Source Monitor"
description = ""
enabled = true
# agent = "lynx"         # optional: whose tools poll the source (default: the action's agent,
                         # else the hub's default agent)
# once = true            # optional: stop after the first successful fire
# expires_at = "2026-12-31T00:00:00Z"   # optional: stop then, telling the owner

[monitor.source]
tool = "google.gmail.search"      # any tool you may call: an adapter, http.request, a WASM skill
input = { query = "from:billing@acme.com newer_than:7d", max_results = 20 }
every_secs = 300                  # poll cadence (the hub sets a minimum)
items = "/messages"               # JSON pointer to the array in the output → watch items by id
# item_id = "/id"                 # pointer within each item (default "/id")
# path = "/snippet"               # pointer to the watched part (default: the whole item)
# text = "/snippet"               # pointer to what {{text}} renders
# Omit \`items\` to watch the whole output as one value (a page, a build's status, a number).

[monitor.condition]               # default: every new item (items) / any change (value)
type = "contains"                 # always | changed | equals | contains | regex | gt | lt | gte | lte | exists | all | any
path = "/snippet"
values = ["invoice"]
# value = "completed"             # equals / gt / lt / gte / lte
# pattern = "INV-\\\\d+"          # regex
# negate = true
# of = [ { type = "gte", path = "/amount", value = 100 }, { type = "changed" } ]   # all / any

# [monitor.decide]                # optional: one yes/no to the decision model before the action
# question = "Is this an invoice that needs paying, not a receipt?"
# yes = "a bill with an amount due"          # optional criteria the model judges by
# no = "a receipt or a payment confirmation"
# min_probability = 0.7

[monitor.action]
type = "notify"                   # run_workflow | run_skill | notify | agent_task
target = "@U0XXXXXXX"             # channel id, or "@U0XXXXXXX" to DM a user
template = "{{id}}: {{text}}"     # {{text}} {{id}} {{value}} {{previous}} {{json:/a/b}}
surface = "slack"                 # where to deliver (optional when the hub has one surface)
# --- or run a skill tool directly, no model turn ---
# type = "run_skill"
# tool = "acme-books.file_invoice"
# input = { gmail_id = "{{id}}" }
# --- or run a workflow with the observation as its trigger payload ---
# type = "run_workflow"
# workflow = "my-workflow"
# --- or run a one-shot agent turn and deliver its reply ---
# type = "agent_task"
# prompt = "Read this and tell me whether it needs action."
# deliver_to = "@U0XXXXXXX"
# surface = "slack"
`;

const NEW_STUBS: Record<MonitorKind, string> = {
  channel: NEW_CHANNEL_STUB,
  source: NEW_SOURCE_STUB,
};

interface ParsedMonitor {
  id?: string;
  name?: string;
  enabled?: boolean;
}

function parseMonitor(toml: string): {
  monitor: ParsedMonitor | null;
  error: string | null;
  errorLine: number | null;
} {
  try {
    const obj = parseToml(toml) as { monitor?: ParsedMonitor };
    return { monitor: obj?.monitor ?? null, error: null, errorLine: null };
  } catch (e) {
    const err = e as { message?: string; line?: number };
    return {
      monitor: null,
      error: err?.message ?? String(e),
      errorLine: typeof err?.line === "number" ? err.line : null,
    };
  }
}

/** Flip `enabled` while preserving the user's formatting and comments. */
function setEnabledInToml(toml: string, enabled: boolean): string {
  if (/^\s*enabled\s*=/m.test(toml)) {
    return toml.replace(/^(\s*enabled\s*=\s*)(true|false)/m, `$1${enabled}`);
  }
  return toml.replace(/(\[monitor\][^\n]*\n)/, `$1enabled = ${enabled}\n`);
}

export default function MonitorDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const isNew = id === "new";

  const [searchParams] = useSearchParams();
  const newKind: MonitorKind = searchParams.get("kind") === "source" ? "source" : "channel";
  const { data: monitorsData } = useMonitors();
  const owner =
    searchParams.get("owner") ??
    monitorsData?.monitors.find((m) => m.id === id)?.owner ??
    null;
  const { data: source, isLoading } = useMonitorSource(isNew ? undefined : id, owner);
  const saveMutation = useSaveMonitor();

  const [toml, setToml] = useState("");
  const [savedSource, setSavedSource] = useState("");
  const [showDiff, setShowDiff] = useState(false);
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  const revisionsQuery = useMonitorRevisions(showHistory && !isNew ? id : undefined, owner);
  const restoreMutation = useRestoreMonitorRevision();

  const [visibilityOverride, setVisibilityOverride] = useState<string | null>(null);
  const listed = monitorsData?.monitors.find((m) => m.id === id && (m.owner ?? null) === owner);
  const loadedVisibility = listed?.visibility ?? (isNew || owner ? "private" : "global");
  const visibility = visibilityOverride ?? loadedVisibility;
  const visibilityDirty = visibilityOverride !== null && visibilityOverride !== loadedVisibility;
  useEffect(() => setVisibilityOverride(null), [id, owner]);

  const existingIds = useMemo(
    () => new Set((monitorsData?.monitors ?? []).map((m) => m.id)),
    [monitorsData],
  );

  useEffect(() => {
    if (isNew) {
      setToml(NEW_STUBS[newKind]);
      setSavedSource(NEW_STUBS[newKind]);
    } else if (source && typeof source === "string") {
      setToml(source);
      setSavedSource(source);
    }
  }, [source, isNew, newKind]);

  const parsed = useMemo(() => parseMonitor(toml), [toml]);
  const isDirty = toml !== savedSource;

  const handleSave = useCallback(
    (nextToml: string = toml) => {
      const result = parseMonitor(nextToml);
      if (result.error) {
        alert("Fix the TOML parse error before saving.");
        return;
      }
      const targetId = result.monitor?.id?.trim();
      if (!targetId) {
        alert("Set a monitor id (the `id` field under [monitor]) before saving.");
        return;
      }
      const idChanged = targetId !== id;
      if (idChanged && existingIds.has(targetId)) {
        if (!confirm(`A monitor "${targetId}" already exists. Overwrite it?`)) return;
      }
      saveMutation.mutate(
        {
          id: targetId,
          toml: nextToml,
          from: idChanged && !isNew ? id : undefined,
          visibility,
          owner: isNew ? undefined : owner,
        },
        {
          onSuccess: () => {
            setSavedSource(nextToml);
            setToml(nextToml);
            setVisibilityOverride(null);
            if (idChanged || isNew) navigate(`/monitors/${targetId}`, { replace: true });
          },
          onError: (err) => alert(`Save failed: ${err instanceof Error ? err.message : String(err)}`),
        },
      );
    },
    [id, isNew, owner, toml, saveMutation, navigate, existingIds, visibility],
  );

  const handleToggleEnabled = useCallback(() => {
    if (parsed.error) {
      alert("Fix the TOML parse error first.");
      return;
    }
    handleSave(setEnabledInToml(toml, !(parsed.monitor?.enabled ?? true)));
  }, [toml, parsed, handleSave]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "s") {
        e.preventDefault();
        handleSave();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [handleSave]);

  if (!isNew && isLoading) return <PageSkeleton />;

  const enabled = parsed.monitor?.enabled ?? true;
  const monitorName = parsed.monitor?.name ?? id ?? "Monitor";
  const status = !isDirty && listed ? listed.status : enabled ? "active" : "paused";
  const hint = !isDirty && listed ? troubleHint(listed) : null;
  const monitorForDialog: MonitorDef | null = isNew
    ? null
    : (listed ??
      (parsed.monitor?.id
        ? {
            id: parsed.monitor.id,
            name: parsed.monitor.name ?? parsed.monitor.id,
            description: "",
            kind: "channel",
            surface: "",
            channel: "",
            filter: "",
            action: "",
            enabled,
            owner,
            status,
          }
        : null));

  return (
    <div className="flex flex-col h-[calc(100vh-3.5rem)]">
      <div className="flex items-center justify-between gap-3 px-4 py-2.5 border-b bg-card shrink-0">
        <div className="flex items-center gap-3 min-w-0">
          <Button variant="ghost" size="icon" className="h-8 w-8 shrink-0" onClick={() => navigate("/monitors")}>
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div className="min-w-0">
            <h1 className="text-sm font-semibold truncate">{monitorName}</h1>
            {parsed.monitor?.id && (
              <span className="text-xs text-muted-foreground font-mono">{parsed.monitor.id}</span>
            )}
          </div>
          <MonitorStatusBadge status={status} className="text-[10px]" />
          {hint && (
            <span className="text-xs text-warning truncate" title={listed?.block?.error ?? hint}>
              {hint}
            </span>
          )}
          {isDirty && <Badge variant="warning" className="text-[10px]">Unsaved</Badge>}
          {parsed.error && <Badge variant="destructive" className="text-[10px]">Parse Error</Badge>}
        </div>

        <div className="flex items-center gap-2 shrink-0">
          <Button
            variant={showDiff ? "secondary" : "outline"}
            size="sm"
            className="h-7 text-xs"
            onClick={() => setShowDiff(!showDiff)}
            disabled={!isDirty}
          >
            <Diff className="h-3.5 w-3.5 mr-1" />Diff
          </Button>
          <Button variant="outline" size="sm" className="h-7 text-xs" onClick={() => setShowHistory(true)} disabled={isNew}>
            <History className="h-3.5 w-3.5 mr-1" />History
          </Button>
          <Button
            variant="outline"
            size="sm"
            className="h-7 text-xs"
            onClick={handleToggleEnabled}
            disabled={isNew || saveMutation.isPending}
            title={enabled ? "Pause this monitor" : "Resume this monitor"}
          >
            {enabled ? <Pause className="h-3.5 w-3.5 mr-1" /> : <Play className="h-3.5 w-3.5 mr-1" />}
            {enabled ? "Pause" : "Resume"}
          </Button>
          <VisibilitySelect
            className="h-7 text-xs"
            value={definitionVisibility(visibility)}
            options={visibilityChoices(!isNew && !owner)}
            onChange={(next) => setVisibilityOverride(definitionWord(next))}
          />
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7 text-destructive hover:text-destructive"
            onClick={() => setShowDeleteDialog(true)}
            disabled={isNew}
          >
            <Trash2 className="h-3.5 w-3.5" />
          </Button>
          <Button
            size="sm"
            className="h-7 text-xs"
            onClick={() => handleSave()}
            disabled={(!isDirty && !visibilityDirty) || !!parsed.error || saveMutation.isPending}
          >
            <Save className="h-3.5 w-3.5 mr-1" />
            {saveMutation.isPending ? "Saving..." : "Save"}
          </Button>
        </div>
      </div>

      <div className="flex-1 min-h-0">
        {showDiff ? (
          <WorkflowDiff original={savedSource} modified={toml} />
        ) : (
          <TomlEditor
            value={toml}
            onChange={setToml}
            parseError={parsed.error}
            parseErrorLine={parsed.errorLine}
          />
        )}
      </div>

      {!isNew && id && (
        <div className="shrink-0 border-t max-h-56 overflow-auto">
          <RecentActivity id={id} owner={owner} />
        </div>
      )}

      {monitorForDialog && (
        <DeleteMonitorDialog
          monitor={monitorForDialog}
          open={showDeleteDialog}
          onOpenChange={setShowDeleteDialog}
          onDeleted={() => navigate("/monitors")}
        />
      )}
      {!isNew && id && (
        <RevisionHistoryDialog
          open={showHistory}
          onOpenChange={setShowHistory}
          title={id}
          revisions={revisionsQuery.data?.revisions}
          isLoading={revisionsQuery.isLoading}
          currentContent={toml}
          getContent={(rev) => monitorsApi.getRevision(id, rev, owner).then((r) => r.content ?? "")}
          onRestore={(rev) =>
            restoreMutation.mutate(
              { id, rev, owner },
              {
                onSuccess: () => setShowHistory(false),
                onError: (err) => alert(`Restore failed: ${err instanceof Error ? err.message : String(err)}`),
              },
            )
          }
          restorePending={restoreMutation.isPending}
        />
      )}
    </div>
  );
}
