import { useState } from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  CheckCircle2,
  Circle,
  ExternalLink,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  AlertTriangle,
  KeyRound,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CardDescription,
} from "@/components/ui/card";
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
import {
  mcpApi,
  type McpInstallReport,
  type McpPresetSummary,
  type McpServerSummary,
  type McpSetupStep,
} from "@/lib/api/mcp";
import { vaultApi } from "@/lib/api/vault";
import { apiErrorMessage } from "@/lib/api-error";
import { useAuthStore } from "@/stores/auth-store";

export default function McpSettingsTab() {
  const queryClient = useQueryClient();
  const installed = useQuery({
    queryKey: ["mcp", "servers"],
    queryFn: () => mcpApi.listServers(),
  });
  const presets = useQuery({
    queryKey: ["mcp", "presets"],
    queryFn: () => mcpApi.listPresets(),
  });

  const refresh = useMutation({
    mutationFn: (id: string) => mcpApi.refresh(id),
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["mcp", "servers"] }),
  });
  const uninstall = useMutation({
    mutationFn: ({ id, purge }: { id: string; purge: boolean }) =>
      mcpApi.uninstall(id, purge),
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ["mcp", "servers"] }),
  });

  const [customOpen, setCustomOpen] = useState(false);
  const installedServers = installed.data?.servers ?? [];
  const presetEntries = presets.data?.presets ?? [];
  const installedIds = new Set(installedServers.map((s) => s.id));
  const invalidateServers = () =>
    queryClient.invalidateQueries({ queryKey: ["mcp", "servers"] });

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <CardTitle>Installed MCP servers</CardTitle>
          <CardDescription>
            Servers connected to the default agent. Tools are exposed as
            <code className="mx-1 rounded bg-muted px-1.5 py-0.5 text-xs">
              mcp.&lt;server&gt;.*
            </code>
            in the agent's tool registry. Install / uninstall is hot — no
            hub restart required.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {installed.isLoading ? (
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              Loading…
            </div>
          ) : installedServers.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              No servers installed yet. Pick one from the catalog below to get
              started.
            </p>
          ) : (
            <div className="space-y-2">
              {installedServers.map((s) => (
                <ServerRow
                  key={s.id}
                  server={s}
                  refreshing={refresh.isPending && refresh.variables === s.id}
                  uninstalling={
                    uninstall.isPending && uninstall.variables?.id === s.id
                  }
                  onRefresh={() => refresh.mutate(s.id)}
                  onUninstall={(purge) => uninstall.mutate({ id: s.id, purge })}
                />
              ))}
            </div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <CardTitle>Catalog</CardTitle>
            <CardDescription>
              Curated presets with endpoint + auth shape baked in. For OAuth
              servers the form below collects the operator-supplied app
              credentials and writes them straight to the vault.
            </CardDescription>
          </div>
          <Button size="sm" variant="outline" onClick={() => setCustomOpen(true)}>
            <Plus className="mr-1 h-3 w-3" /> Custom URL
          </Button>
        </CardHeader>
        <CardContent className="grid gap-3 md:grid-cols-2 lg:grid-cols-3">
          {presetEntries.map((p) => (
            <PresetCard
              key={p.id}
              preset={p}
              installed={installedIds.has(p.id)}
              onInstalled={invalidateServers}
            />
          ))}
        </CardContent>
      </Card>

      <CustomInstallDialog
        open={customOpen}
        onOpenChange={setCustomOpen}
        onInstalled={invalidateServers}
      />
    </div>
  );
}

interface ServerRowProps {
  server: McpServerSummary;
  refreshing: boolean;
  uninstalling: boolean;
  onRefresh: () => void;
  onUninstall: (purge: boolean) => void;
}

function ServerRow({
  server,
  refreshing,
  uninstalling,
  onRefresh,
  onUninstall,
}: ServerRowProps) {
  return (
    <div className="flex items-center justify-between rounded border p-3">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <CheckCircle2 className="h-4 w-4 text-green-600" />
          <span className="font-medium">{server.id}</span>
          <Badge variant="outline" className="text-xs">
            {server.auth_kind}
          </Badge>
          <Badge variant="outline" className="text-xs">
            {server.tool_count} tool{server.tool_count === 1 ? "" : "s"}
          </Badge>
          {server.installed_via && (
            <Badge variant="outline" className="text-xs">
              {server.installed_via}
            </Badge>
          )}
        </div>
        <div className="truncate text-xs text-muted-foreground">
          {server.endpoint}
        </div>
      </div>
      <div className="flex items-center gap-2">
        <Button
          size="sm"
          variant="outline"
          onClick={onRefresh}
          disabled={refreshing}
        >
          {refreshing ? (
            <Loader2 className="h-3 w-3 animate-spin" />
          ) : (
            <RefreshCw className="h-3 w-3" />
          )}
          <span className="ml-1">Refresh</span>
        </Button>
        <Button
          size="sm"
          variant="outline"
          className="text-destructive"
          disabled={uninstalling}
          onClick={() => {
            const purge = window.confirm(
              "Also purge any per-user OAuth tokens for this server? Operator-supplied client credentials are preserved either way.",
            );
            onUninstall(purge);
          }}
        >
          {uninstalling ? (
            <Loader2 className="h-3 w-3 animate-spin" />
          ) : (
            <Trash2 className="h-3 w-3" />
          )}
          <span className="ml-1">Uninstall</span>
        </Button>
      </div>
    </div>
  );
}

interface PresetCardProps {
  preset: McpPresetSummary;
  installed: boolean;
  onInstalled: () => void;
}

function PresetCard({ preset, installed, onInstalled }: PresetCardProps) {
  const isAuthenticated = useAuthStore((s) => s.isAuthenticated);
  const [installReport, setInstallReport] = useState<McpInstallReport | null>(
    null,
  );

  const install = useMutation({
    mutationFn: () => mcpApi.install({ preset: preset.id }),
    onSuccess: (report) => {
      setInstallReport(report);
      if (report.status === "installed") {
        onInstalled();
      }
    },
  });
  const authorize = useMutation({
    mutationFn: () => mcpApi.authorize(preset.id),
    onSuccess: (report) => {
      window.open(report.authorization_url, "_blank", "noopener,noreferrer");
    },
  });

  const needsCredentials = installReport?.status === "needs_credentials";

  return (
    <div className="flex flex-col rounded border p-4">
      <div className="mb-2 flex items-center gap-2">
        {installed ? (
          <CheckCircle2 className="h-4 w-4 text-green-600" />
        ) : (
          <Circle className="h-4 w-4 text-muted-foreground" />
        )}
        <span className="font-medium">{preset.label}</span>
        <Badge variant="outline" className="ml-auto text-xs">
          {preset.auth_kind}
        </Badge>
      </div>
      <div className="mb-3 truncate text-xs text-muted-foreground">
        {preset.endpoint}
      </div>

      {preset.auth_kind.startsWith("oauth2") && preset.oauth_app_registration_url && (
        <a
          href={preset.oauth_app_registration_url}
          target="_blank"
          rel="noopener noreferrer"
          className="mb-2 inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
        >
          Register OAuth app on {preset.label}
          <ExternalLink className="h-3 w-3" />
        </a>
      )}

      {needsCredentials && (
        <CredentialsForm
          preset={preset}
          missing={installReport.missing}
          instructions={installReport.instructions}
          setupSteps={installReport.setup_steps ?? preset.setup_steps}
          onSeeded={() => install.mutate()}
        />
      )}

      <div className="mt-auto flex gap-2">
        {!installed && !needsCredentials && (
          <Button
            size="sm"
            onClick={() => install.mutate()}
            disabled={install.isPending}
          >
            {install.isPending && (
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
            )}
            Install
          </Button>
        )}
        {preset.auth_kind.startsWith("oauth2") && (installed || needsCredentials) && (
          <Button
            size="sm"
            variant="outline"
            onClick={() => authorize.mutate()}
            disabled={authorize.isPending || !isAuthenticated}
          >
            {authorize.isPending && (
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
            )}
            Authorize
          </Button>
        )}
      </div>
    </div>
  );
}

interface CredentialsFormProps {
  preset: McpPresetSummary;
  missing: string[];
  instructions: string;
  setupSteps: McpSetupStep[];
  onSeeded: () => void;
}

/// Inline form that writes operator-supplied OAuth client credentials
/// (or a PAT) straight to the vault, then triggers re-install. The
/// numbered walkthrough (`setupSteps`) shows the user exactly what to
/// do next; setup steps tied to a specific `vault_path` get rendered
/// inline next to the input row so the form reads top-to-bottom.
function CredentialsForm({
  preset,
  missing,
  instructions,
  setupSteps,
  onSeeded,
}: CredentialsFormProps) {
  const paths = missing.length > 0 ? missing : preset.required_vault_paths;
  const [values, setValues] = useState<Record<string, string>>(() => {
    const seed: Record<string, string> = {};
    for (const p of paths) {
      seed[p] = pathDefault(p, preset);
    }
    return seed;
  });
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    setSubmitting(true);
    setError(null);
    try {
      await Promise.all(
        paths
          .filter((p) => (values[p] ?? "").trim() !== "")
          .map((p) => vaultApi.set(p, values[p] ?? "")),
      );
      onSeeded();
    } catch (e) {
      setError(apiErrorMessage(e));
    } finally {
      setSubmitting(false);
    }
  };

  // Steps that target a specific input get rendered inline next to
  // the matching row. The rest (typically: register the app, copy
  // the credentials) appear in the numbered preamble at the top.
  const stepsByPath = new Map<string, McpSetupStep>();
  const preambleSteps: McpSetupStep[] = [];
  for (const s of setupSteps) {
    if (s.vault_path) {
      stepsByPath.set(s.vault_path, s);
    } else {
      preambleSteps.push(s);
    }
  }

  return (
    <div className="mb-3 space-y-2 rounded border border-amber-300 bg-amber-50 p-3 text-xs">
      <div className="flex items-center gap-1 font-medium text-amber-900">
        <AlertTriangle className="h-3 w-3" />
        Credentials needed
      </div>
      <div className="text-amber-900">{instructions}</div>
      {preambleSteps.length > 0 && (
        <ol className="ml-4 list-decimal space-y-1 text-amber-900">
          {preambleSteps.map((s, i) => (
            <li key={`preamble-${i}`}>
              <span className="font-medium">{s.title}.</span>{" "}
              <span>{s.instructions}</span>
              {s.url && (
                <>
                  {" "}
                  <a
                    href={s.url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="inline-flex items-center gap-1 underline"
                  >
                    Open
                    <ExternalLink className="h-3 w-3" />
                  </a>
                </>
              )}
            </li>
          ))}
        </ol>
      )}
      <div className="space-y-2">
        {paths.map((p) => {
          const step = stepsByPath.get(p);
          return (
            <label key={p} className="flex flex-col gap-1">
              <span className="font-mono text-[11px] text-amber-900">{p}</span>
              {step && (
                <span className="text-amber-900">{step.instructions}</span>
              )}
              <Input
                type={isSecretPath(p) ? "password" : "text"}
                value={values[p] ?? ""}
                placeholder={pathPlaceholder(p, preset)}
                onChange={(e) =>
                  setValues((v) => ({ ...v, [p]: e.target.value }))
                }
                className="h-7 text-xs"
              />
            </label>
          );
        })}
      </div>
      {error && <div className="text-red-700">{error}</div>}
      <Button size="sm" onClick={submit} disabled={submitting}>
        {submitting ? (
          <Loader2 className="mr-1 h-3 w-3 animate-spin" />
        ) : (
          <KeyRound className="mr-1 h-3 w-3" />
        )}
        Save credentials and continue
      </Button>
    </div>
  );
}

function isSecretPath(path: string): boolean {
  return (
    path.endsWith("/client_secret") ||
    path.endsWith("/token") ||
    path.endsWith("/api_key")
  );
}

/// Suggest a default value for an OAuth `redirect_uri` if the
/// backend computed one from `[hub].public_url`. Other paths get an
/// empty string — the operator types in their app credentials.
function pathDefault(path: string, preset: McpPresetSummary): string {
  if (path.endsWith("/redirect_uri") && preset.suggested_redirect_uri) {
    return preset.suggested_redirect_uri;
  }
  return "";
}

function pathPlaceholder(path: string, preset: McpPresetSummary): string {
  if (path.endsWith("/redirect_uri")) {
    return preset.suggested_redirect_uri
      ? "(suggested value pre-filled — paste this into your vendor app config)"
      : "configure [hub].public_url to auto-suggest, or enter manually";
  }
  if (path.endsWith("/client_id")) return "from your vendor OAuth app";
  if (path.endsWith("/client_secret")) return "from your vendor OAuth app";
  if (path.endsWith("/token")) return "personal access token";
  if (path.endsWith("/site_url")) return "https://your-instance.example.com";
  return "";
}

interface CustomInstallDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onInstalled: () => void;
}

/// Custom-URL install — for any vendor's MCP endpoint not in the
/// curated catalog. Two auth flavours: a PAT pulled from a vault
/// path (simplest), or full OAuth 2.1 (requires you to register a
/// matching `mcp_servers::oauth_bridge` provider in the hub source).
function CustomInstallDialog({
  open,
  onOpenChange,
  onInstalled,
}: CustomInstallDialogProps) {
  const [serverId, setServerId] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [authKind, setAuthKind] = useState<"pat" | "oauth2">("pat");
  const [vaultPath, setVaultPath] = useState("");
  const [oauthProvider, setOauthProvider] = useState("");
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setServerId("");
    setEndpoint("");
    setAuthKind("pat");
    setVaultPath("");
    setOauthProvider("");
    setError(null);
  };

  const install = useMutation({
    mutationFn: () =>
      mcpApi.install({
        server_id: serverId,
        custom: {
          endpoint,
          auth:
            authKind === "pat"
              ? { kind: "pat", vault_path: vaultPath }
              : {
                  kind: "oauth2",
                  binding: "user",
                  provider: oauthProvider,
                  scopes: [],
                },
        },
      }),
    onSuccess: (report) => {
      onInstalled();
      if (report.status === "installed") {
        onOpenChange(false);
        reset();
      } else {
        setError(report.instructions);
      }
    },
    onError: (e) =>
      setError(e instanceof Error ? e.message : "install failed"),
  });

  const submit = () => {
    if (!serverId || !endpoint) {
      setError("server_id and endpoint are required");
      return;
    }
    if (authKind === "pat" && !vaultPath) {
      setError("PAT auth requires a vault_path");
      return;
    }
    if (authKind === "oauth2" && !oauthProvider) {
      setError("OAuth2 auth requires a provider name");
      return;
    }
    setError(null);
    install.mutate();
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Install custom MCP server</DialogTitle>
          <DialogDescription>
            Connect any vendor's MCP-over-HTTPS endpoint. PAT auth reads a
            bearer token from the vault on every call. OAuth 2.1 requires
            a matching provider implementation already registered in the
            hub source.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3 py-2">
          <label className="flex flex-col gap-1 text-xs">
            <span className="font-medium">server_id</span>
            <Input
              value={serverId}
              onChange={(e) => setServerId(e.target.value)}
              placeholder="e.g. notion"
            />
          </label>
          <label className="flex flex-col gap-1 text-xs">
            <span className="font-medium">endpoint</span>
            <Input
              value={endpoint}
              onChange={(e) => setEndpoint(e.target.value)}
              placeholder="https://mcp.vendor.com/v1"
            />
          </label>
          <label className="flex flex-col gap-1 text-xs">
            <span className="font-medium">auth kind</span>
            <select
              value={authKind}
              onChange={(e) => setAuthKind(e.target.value as "pat" | "oauth2")}
              className="h-9 rounded border bg-background px-2 text-sm"
            >
              <option value="pat">PAT (bearer token from vault)</option>
              <option value="oauth2">OAuth 2.1</option>
            </select>
          </label>
          {authKind === "pat" ? (
            <label className="flex flex-col gap-1 text-xs">
              <span className="font-medium">vault_path</span>
              <Input
                value={vaultPath}
                onChange={(e) => setVaultPath(e.target.value)}
                placeholder="mcp/v1/notion/token"
              />
            </label>
          ) : (
            <label className="flex flex-col gap-1 text-xs">
              <span className="font-medium">provider</span>
              <Input
                value={oauthProvider}
                onChange={(e) => setOauthProvider(e.target.value)}
                placeholder="must match an OAuthProvider impl (e.g. linear, atlassian)"
              />
            </label>
          )}
          {error && <div className="text-xs text-red-600">{error}</div>}
        </div>
        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => {
              reset();
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button onClick={submit} disabled={install.isPending}>
            {install.isPending && (
              <Loader2 className="mr-1 h-3 w-3 animate-spin" />
            )}
            Install
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
