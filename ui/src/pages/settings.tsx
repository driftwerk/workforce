import { useState, useEffect } from "react";
import {
  Settings,
  Shield,
  Save,
  Loader2,
  CheckCircle,
  XCircle,
} from "lucide-react";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { SecretInput } from "@/components/subscriptions/secret-input";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { ConfigOverlayEditor } from "@/components/config/config-overlay-editor";
import { useRawConfig, useSaveRawConfig } from "@/hooks/use-config";
import { useVaultSet } from "@/hooks/use-vault";
import { apiErrorMessage } from "@/lib/api-error";
import { useQuery } from "@tanstack/react-query";
import { authConfigApi } from "@/lib/api/auth-config";
import McpSettingsTab from "@/pages/settings-mcp";

export default function SettingsPage() {
  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-3">
        <div className="rounded-full bg-muted p-2">
          <Settings className="h-5 w-5 text-muted-foreground" />
        </div>
        <div>
          <h1 className="text-lg font-semibold">Settings</h1>
          <p className="text-sm text-muted-foreground">
            System configuration and preferences.
          </p>
        </div>
      </div>

      <Tabs defaultValue="config">
        <TabsList>
          <TabsTrigger value="config">Config</TabsTrigger>
          <TabsTrigger value="mcp">MCP Servers</TabsTrigger>
          <TabsTrigger value="oidc">OIDC / SSO</TabsTrigger>
        </TabsList>

        <TabsContent value="config">
          <div className="mt-4">
            <ConfigOverlayEditor />
          </div>
        </TabsContent>

        <TabsContent value="mcp">
          <div className="mt-4">
            <McpSettingsTab />
          </div>
        </TabsContent>

        <TabsContent value="oidc">
          <div className="mt-4">
            <OidcConfigSection />
          </div>
        </TabsContent>
      </Tabs>
    </div>
  );
}

// ── OIDC Configuration ──────────────────────────────────────────────────────

interface OidcFormState {
  enabled: boolean;
  display_name: string;
  issuer_url: string;
  client_id: string;
  client_secret: string;
  redirect_uri: string;
  allowed_email_domains: string;
  default_role: string;
  auto_create_users: boolean;
}

const EMPTY_OIDC: OidcFormState = {
  enabled: false,
  display_name: "Keycloak",
  issuer_url: "",
  client_id: "",
  client_secret: "",
  redirect_uri: "",
  allowed_email_domains: "",
  default_role: "member",
  auto_create_users: true,
};

function parseOidcFromToml(toml: string): Partial<OidcFormState> {
  const result: Partial<OidcFormState> = {};
  let inOidc = false;

  for (const line of toml.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "[gateway.oidc]") {
      inOidc = true;
      continue;
    }
    if (trimmed.startsWith("[") && inOidc) break;
    if (!inOidc || trimmed.startsWith("#") || !trimmed.includes("=")) continue;

    const eqIdx = trimmed.indexOf("=");
    const key = trimmed.slice(0, eqIdx).trim();
    let val = trimmed.slice(eqIdx + 1).trim();

    if (val.startsWith('"') && val.endsWith('"')) val = val.slice(1, -1);

    switch (key) {
      case "enabled":
        result.enabled = val === "true";
        break;
      case "display_name":
        result.display_name = val;
        break;
      case "issuer_url":
        result.issuer_url = val;
        break;
      case "client_id":
        result.client_id = val;
        break;
      case "client_secret":
        result.client_secret = val;
        break;
      case "redirect_uri":
        result.redirect_uri = val;
        break;
      case "allowed_email_domains":
        result.allowed_email_domains = val
          .replace(/[\[\]"]/g, "")
          .split(",")
          .map((s: string) => s.trim())
          .filter(Boolean)
          .join(", ");
        break;
      case "default_role":
        result.default_role = val;
        break;
      case "auto_create_users":
        result.auto_create_users = val === "true";
        break;
    }
  }
  return result;
}

function buildOidcToml(form: OidcFormState): string {
  const lines = [
    "[gateway.oidc]",
    `enabled = ${form.enabled}`,
    `display_name = "${form.display_name}"`,
    `issuer_url = "${form.issuer_url}"`,
    `client_id = "${form.client_id}"`,
    `client_secret = "${form.client_secret}"`,
    `redirect_uri = "${form.redirect_uri}"`,
  ];
  if (form.allowed_email_domains.trim()) {
    const domains = form.allowed_email_domains
      .split(",")
      .map((s) => `"${s.trim()}"`)
      .join(", ");
    lines.push(`allowed_email_domains = [${domains}]`);
  } else {
    lines.push("allowed_email_domains = []");
  }
  lines.push(`default_role = "${form.default_role}"`);
  lines.push(`auto_create_users = ${form.auto_create_users}`);
  return lines.join("\n");
}

function replaceOidcSection(toml: string, newSection: string): string {
  const lines = toml.split("\n");
  let start = -1;
  let end = lines.length;

  for (let i = 0; i < lines.length; i++) {
    if (lines[i]?.trim() === "[gateway.oidc]") {
      start = i;
      for (let j = i + 1; j < lines.length; j++) {
        if (lines[j]?.trim().startsWith("[")) {
          end = j;
          break;
        }
      }
      break;
    }
  }

  if (start >= 0) {
    lines.splice(start, end - start, newSection);
  } else {
    lines.push("", newSection);
  }
  return lines.join("\n");
}

function OidcConfigSection() {
  const { data: rawConfig, isLoading } = useRawConfig();
  const saveConfig = useSaveRawConfig();
  const vaultSet = useVaultSet();
  const { data: authConfig } = useQuery({
    queryKey: ["auth-config"],
    queryFn: authConfigApi.get,
    staleTime: 30_000,
  });

  const [form, setForm] = useState<OidcFormState>(EMPTY_OIDC);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (rawConfig) {
      const parsed = parseOidcFromToml(rawConfig as string);
      setForm((f) => ({ ...f, ...parsed }));
    }
  }, [rawConfig]);

  const isActive = !!authConfig?.oidc;

  const update = (field: keyof OidcFormState) => (
    e: React.ChangeEvent<HTMLInputElement | HTMLSelectElement>,
  ) => setForm((f) => ({ ...f, [field]: e.target.value }));

  const toggle = (field: keyof OidcFormState) => () =>
    setForm((f) => ({ ...f, [field]: !f[field] }));

  async function handleSave() {
    setSaved(false);
    setError(null);
    try {
      let secretRef = form.client_secret;

      if (form.client_secret && !form.client_secret.startsWith("${vault:")) {
        await vaultSet.mutateAsync({
          path: "oidc_client_secret",
          value: form.client_secret,
        });
        secretRef = "${vault:oidc_client_secret}";
      }

      const oidcToml = buildOidcToml({ ...form, client_secret: secretRef });
      const updatedConfig = replaceOidcSection(rawConfig as string, oidcToml);
      await saveConfig.mutateAsync(updatedConfig);
      setForm((f) => ({ ...f, client_secret: secretRef }));
      setSaved(true);
      setTimeout(() => setSaved(false), 3000);
    } catch (e) {
      setError(apiErrorMessage(e));
    }
  }

  if (isLoading) {
    return (
      <div className="flex justify-center py-12">
        <Loader2 className="h-5 w-5 animate-spin" />
      </div>
    );
  }

  const defaultRedirect = `${window.location.origin}/api/auth/oidc/callback`;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-3">
              <Shield className="h-5 w-5 text-muted-foreground" />
              <div>
                <CardTitle className="text-base">OIDC / SSO Configuration</CardTitle>
                <CardDescription>
                  Connect to Keycloak or another OIDC provider for single sign-on.
                </CardDescription>
              </div>
            </div>
            <Badge
              variant={isActive ? "default" : "outline"}
              className={isActive ? "bg-green-600" : ""}
            >
              {isActive ? (
                <><CheckCircle className="h-3 w-3 mr-1" />Active</>
              ) : (
                <><XCircle className="h-3 w-3 mr-1" />Inactive</>
              )}
            </Badge>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex items-center gap-2">
            <input
              type="checkbox"
              id="oidc-enabled"
              checked={form.enabled}
              onChange={toggle("enabled")}
              className="rounded"
            />
            <label htmlFor="oidc-enabled" className="text-sm font-medium">
              Enable OIDC
            </label>
          </div>

          <div className="grid grid-cols-2 gap-4">
            <div className="space-y-1.5">
              <label className="text-sm font-medium">Display Name</label>
              <Input
                value={form.display_name}
                onChange={update("display_name")}
                placeholder="Keycloak"
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-sm font-medium">Issuer URL</label>
              <Input
                value={form.issuer_url}
                onChange={update("issuer_url")}
                placeholder="https://keycloak.example.com/auth/realms/myrealm"
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-sm font-medium">Client ID</label>
              <Input
                value={form.client_id}
                onChange={update("client_id")}
                placeholder="workforce-client"
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-sm font-medium">Client Secret</label>
              <SecretInput
                value={form.client_secret}
                onChange={(v) => setForm((f) => ({ ...f, client_secret: v }))}
                placeholder="Enter client secret"
                masksVaultRef
              />
            </div>
          </div>

          <div className="space-y-1.5">
            <label className="text-sm font-medium">Redirect URI</label>
            <Input
              value={form.redirect_uri}
              onChange={update("redirect_uri")}
              placeholder={defaultRedirect}
            />
            <p className="text-xs text-muted-foreground">
              Must match the redirect URI registered in your IdP. Suggested: {defaultRedirect}
            </p>
          </div>

          <div className="grid grid-cols-2 gap-4">
            <div className="space-y-1.5">
              <label className="text-sm font-medium">Allowed Email Domains</label>
              <Input
                value={form.allowed_email_domains}
                onChange={update("allowed_email_domains")}
                placeholder="example.com, company.io (empty = allow all)"
              />
            </div>
            <div className="space-y-1.5">
              <label className="text-sm font-medium">Default Role</label>
              <select
                value={form.default_role}
                onChange={update("default_role")}
                className="w-full rounded-md border bg-background px-3 py-2 text-sm"
              >
                <option value="member">Member</option>
                <option value="manager">Manager</option>
                <option value="system_admin">System Admin</option>
              </select>
            </div>
          </div>

          <div className="flex items-center gap-2">
            <input
              type="checkbox"
              id="oidc-auto-create"
              checked={form.auto_create_users}
              onChange={toggle("auto_create_users")}
              className="rounded"
            />
            <label htmlFor="oidc-auto-create" className="text-sm font-medium">
              Auto-create users on first login
            </label>
          </div>

          <div className="flex items-center gap-2 pt-2">
            <Button
              onClick={handleSave}
              disabled={saveConfig.isPending || vaultSet.isPending}
            >
              {saveConfig.isPending || vaultSet.isPending ? (
                <Loader2 className="h-4 w-4 animate-spin mr-1" />
              ) : (
                <Save className="h-4 w-4 mr-1" />
              )}
              Save
            </Button>
            {saved && (
              <span className="text-sm text-green-600">
                Saved. Restart the hub to apply OIDC changes.
              </span>
            )}
            {error && <span className="text-sm text-destructive">{error}</span>}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
