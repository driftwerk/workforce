import { useEffect, useState, type ReactNode } from "react";
import { Link, useLocation } from "react-router";
import {
  Bot,
  Code,
  Globe,
  Mail,
  Mic,
  Receipt,
  Send,
  Ticket,
  Loader2,
  Save,
  Trash2,
  Plus,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { InlineCode } from "@/components/ui/inline-code";
import { Input } from "@/components/ui/input";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { PageSkeleton } from "@/components/loading-skeleton";
import { Skeleton } from "@/components/ui/skeleton";
import {
  useMyIntegrations,
  useMyVault,
  useSetMyVault,
  useDeleteMyVault,
  useDeleteMySecret,
  useMoveMyVault,
} from "@/hooks/use-my-settings";
import { useVaultCatalog } from "@/hooks/use-vault";
import type { IntegrationStatus } from "@/lib/api/my-settings";
import { VaultTable } from "@/components/vault/vault-table";
import { VaultMoveDialog } from "@/components/vault/vault-move-dialog";
import { VaultSecretForm } from "@/components/vault/vault-secret-form";
import { RemoteMcpTokensCard } from "@/components/remote-mcp/remote-mcp-tokens-card";
import type { VaultEntryView, VaultTier } from "@/lib/api/vault";
import { apiErrorMessage } from "@/lib/api-error";
import { SecretInput } from "@/components/subscriptions/secret-input";
import { LoginStatusBadge } from "@/components/subscriptions/login-status-badge";
import { DisconnectButton, LoginConnect } from "@/components/subscriptions/login-connect";
import { useAgentLogins, useMySubscriptions } from "@/hooks/use-subscriptions";
import type { LoginStatus, MySubscriptions, SubscriptionKind } from "@/lib/api/subscriptions";
import { CHATGPT_DIRECT_OFF, KIND_LABELS, ownLoginStatus } from "@/lib/subscriptions/status";

// ── Status Badge ────────────────────────────────────────────────────────────

function ScopeBadge({ scope }: { scope: IntegrationStatus["scope"] }) {
  switch (scope) {
    case "personal":
      return <Badge className="bg-green-600 hover:bg-green-700">Personal</Badge>;
    case "shared":
      return <Badge variant="secondary">Shared</Badge>;
    case "none":
      return <Badge variant="outline">Not configured</Badge>;
  }
}

function scopeDescription(scope: IntegrationStatus["scope"]): string {
  switch (scope) {
    case "personal":
      return "Using your personal credentials";
    case "shared":
      return "Using shared credentials";
    case "none":
      return "No credentials configured";
  }
}

// ── Subscription Sections ───────────────────────────────────────────────────

const SUBSCRIPTION_CARDS: Record<SubscriptionKind, { icon: typeof Code; description: string }> = {
  claude: {
    icon: Code,
    description:
      "Your Claude plan (Pro/Max) login for agents, coding tasks and workflow delegation",
  },
  chatgpt: {
    icon: Bot,
    description: "Your ChatGPT plan login for agents on ChatGPT and Codex",
  },
};

/** A link to the agent's own-login card, where the agent's policy has one. */
function ManageOnAgent({ agentId, status }: { agentId: string; status: LoginStatus }) {
  const view = useAgentLogins(agentId);
  if (!view.data?.policy.tiers.includes("agent_user")) return null;
  return (
    <Button size="sm" variant="outline" asChild>
      <Link to={`/agents/${encodeURIComponent(agentId)}?tab=logins`}>
        {status.state === "invalid" ? "Reconnect" : "Manage"}
      </Link>
    </Button>
  );
}

function SubscriptionSection({ kind, mine }: { kind: SubscriptionKind; mine: MySubscriptions }) {
  const { icon, description } = SUBSCRIPTION_CARDS[kind];
  const logins = mine.logins.filter((login) => login.kind === kind);
  const perAgent = logins.filter((login) => login.tier === "agent_user" && login.agent_id);
  const status = ownLoginStatus(logins, kind, "global_user");

  return (
    <IntegrationCard
      id={kind}
      icon={icon}
      title={KIND_LABELS[kind]}
      description={description}
      badge={<LoginStatusBadge status={status} />}
      error={null}
    >
      <div className="space-y-4">
        {kind === "chatgpt" && !mine.chatgpt_direct && (
          <p className="text-sm text-muted-foreground">{CHATGPT_DIRECT_OFF}</p>
        )}
        <LoginConnect kind={kind} target={{ tier: "global_user" }} connected={status.state !== "not_set"} />
        {perAgent.length > 0 && (
          <ul className="space-y-2 border-t pt-3">
            {perAgent.map((login) => (
              <li key={login.agent_id} className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-sm">
                  Set on <span className="font-medium">{login.agent_name ?? login.agent_id}</span>: used
                  there instead of your global login
                </span>
                <div className="flex items-center gap-2">
                  <LoginStatusBadge status={login.status} />
                  <ManageOnAgent agentId={login.agent_id!} status={login.status} />
                  <DisconnectButton
                    kind={kind}
                    target={{ tier: "agent_user", agent_id: login.agent_id! }}
                  />
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>
    </IntegrationCard>
  );
}

function SubscriptionSections() {
  const { data, isLoading, error } = useMySubscriptions();
  const { hash } = useLocation();
  useEffect(() => {
    if (data && hash) document.getElementById(hash.slice(1))?.scrollIntoView();
  }, [data, hash]);

  if (isLoading) {
    return (
      <>
        <Skeleton className="h-40 rounded-lg" />
        <Skeleton className="h-40 rounded-lg" />
      </>
    );
  }
  if (error || !data) {
    return (
      <p className="text-sm text-destructive">
        Failed to load your Claude and ChatGPT logins: {apiErrorMessage(error)}
      </p>
    );
  }
  return (
    <>
      <SubscriptionSection kind="claude" mine={data} />
      <SubscriptionSection kind="chatgpt" mine={data} />
    </>
  );
}

// ── Google Section ──────────────────────────────────────────────────────────

function GoogleSection({ status }: { status: IntegrationStatus }) {
  const vault = useIntegrationVault();

  const googleKeys = [
    "skills/google/workspaces/default/refresh_token",
    "skills/google/workspaces/default/access_token",
    "skills/google/workspaces/default/email",
    "skills/google/workspaces/default/expires_at",
    "skills/google/workspaces/default/scopes",
  ];

  function handleDisconnect() {
    if (!confirm("Disconnect your Google account? You can reconnect later.")) return;
    vault.remove(googleKeys);
  }

  return (
    <IntegrationCard
      icon={Globe}
      title="Google"
      description="Connect your Google account for Gmail, Calendar, and Drive"
      status={status}
      error={vault.error}
    >
      <div className="space-y-3">
        {status.email && (
          <p className="text-sm">
            Connected as: <span className="font-medium">{status.email}</span>
          </p>
        )}
        {status.scope === "personal" ? (
          <Button
            size="sm"
            variant="outline"
            onClick={handleDisconnect}
            disabled={vault.removing}
          >
            <Trash2 className="h-4 w-4 mr-1" />
            Disconnect
          </Button>
        ) : (
          <p className="text-sm text-muted-foreground">
            To connect your Google account, ask the agent to run{" "}
            <InlineCode>google login</InlineCode>
          </p>
        )}
      </div>
    </IntegrationCard>
  );
}

// ── FreeAgent Section ─────────────────────────────────────────────────────────

function FreeAgentSection({ status }: { status: IntegrationStatus }) {
  const vault = useIntegrationVault();

  const freeAgentKeys = [
    "skills/freeagent/workspaces/default/refresh_token",
    "skills/freeagent/workspaces/default/access_token",
    "skills/freeagent/workspaces/default/expires_at",
    "skills/freeagent/workspaces/default/scopes",
    "skills/freeagent/workspaces/default/company",
  ];

  function handleDisconnect() {
    if (!confirm("Disconnect your FreeAgent company? You can reconnect later.")) return;
    vault.remove(freeAgentKeys);
  }

  return (
    <IntegrationCard
      icon={Receipt}
      title="FreeAgent"
      description="Connect a FreeAgent company for invoices, bills, expenses, and time tracking"
      status={status}
      error={vault.error}
    >
      <div className="space-y-3">
        {status.scope === "personal" ? (
          <Button
            size="sm"
            variant="outline"
            onClick={handleDisconnect}
            disabled={vault.removing}
          >
            <Trash2 className="h-4 w-4 mr-1" />
            Disconnect
          </Button>
        ) : (
          <p className="text-sm text-muted-foreground">
            To connect your FreeAgent company, ask the agent to run{" "}
            <InlineCode>freeagent login</InlineCode>
          </p>
        )}
      </div>
    </IntegrationCard>
  );
}

// ── Email Section ───────────────────────────────────────────────────────────

function EmailSection({ status }: { status: IntegrationStatus }) {
  const [form, setForm] = useState({
    imap_host: "",
    imap_port: "993",
    smtp_host: "",
    smtp_port: "587",
    username: "",
    password: "",
    display_name: "",
    tls: true,
  });
  const vault = useIntegrationVault();

  const prefix = "skills/email/accounts/default";

  function handleSave() {
    vault.save([
      { path: `${prefix}/imap_host`, value: form.imap_host },
      { path: `${prefix}/imap_port`, value: form.imap_port },
      { path: `${prefix}/smtp_host`, value: form.smtp_host },
      { path: `${prefix}/smtp_port`, value: form.smtp_port },
      { path: `${prefix}/username`, value: form.username },
      { path: `${prefix}/password`, value: form.password },
      { path: `${prefix}/display_name`, value: form.display_name },
      { path: `${prefix}/tls`, value: form.tls ? "true" : "false" },
    ]);
  }

  function handleRemove() {
    if (!confirm("Remove your email configuration?")) return;
    const keys = [
      "imap_host", "imap_port", "smtp_host", "smtp_port",
      "username", "password", "display_name", "tls",
    ];
    vault.remove(keys.map((key) => `${prefix}/${key}`));
  }

  const update = (field: string) => (e: React.ChangeEvent<HTMLInputElement>) =>
    setForm((f) => ({ ...f, [field]: e.target.value }));

  return (
    <IntegrationCard
      icon={Mail}
      title="Email"
      description="IMAP/SMTP credentials for email access"
      status={status}
      error={vault.error}
    >
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="text-sm font-medium">IMAP Host</label>
            <Input value={form.imap_host} onChange={update("imap_host")} placeholder="imap.gmail.com" />
          </div>
          <div>
            <label className="text-sm font-medium">IMAP Port</label>
            <Input value={form.imap_port} onChange={update("imap_port")} placeholder="993" />
          </div>
          <div>
            <label className="text-sm font-medium">SMTP Host</label>
            <Input value={form.smtp_host} onChange={update("smtp_host")} placeholder="smtp.gmail.com" />
          </div>
          <div>
            <label className="text-sm font-medium">SMTP Port</label>
            <Input value={form.smtp_port} onChange={update("smtp_port")} placeholder="587" />
          </div>
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="text-sm font-medium">Username</label>
            <Input value={form.username} onChange={update("username")} placeholder="you@example.com" />
          </div>
          <div>
            <label className="text-sm font-medium">Password</label>
            <SecretInput value={form.password} onChange={(v) => setForm((f) => ({ ...f, password: v }))} placeholder="App password" />
          </div>
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="text-sm font-medium">Display Name</label>
            <Input value={form.display_name} onChange={update("display_name")} placeholder="Your Name" />
          </div>
          <div className="flex items-end gap-2 pb-1">
            <input
              type="checkbox"
              id="tls"
              checked={form.tls}
              onChange={(e) => setForm((f) => ({ ...f, tls: e.target.checked }))}
              className="rounded"
            />
            <label htmlFor="tls" className="text-sm font-medium">TLS</label>
          </div>
        </div>
        <div className="flex gap-2">
          <Button
            size="sm"
            onClick={handleSave}
            disabled={!form.username || !form.password || vault.saving}
          >
            {vault.saving ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <Save className="h-4 w-4 mr-1" />}
            Save
          </Button>
          {status.scope === "personal" && (
            <Button size="sm" variant="outline" onClick={handleRemove} disabled={vault.removing}>
              <Trash2 className="h-4 w-4 mr-1" />
              Remove
            </Button>
          )}
        </div>
      </div>
    </IntegrationCard>
  );
}

// ── Jira Section ────────────────────────────────────────────────────────────

function JiraSection({ status }: { status: IntegrationStatus }) {
  const [form, setForm] = useState({ email: "", api_token: "", site: "" });
  const vault = useIntegrationVault();

  function handleSave() {
    vault.save([
      { path: "skills/jira/email", value: form.email },
      { path: "skills/jira/api_token", value: form.api_token },
      { path: "skills/jira/site", value: form.site },
    ]);
  }

  function handleRemove() {
    if (!confirm("Remove your Jira configuration?")) return;
    vault.remove(["skills/jira/email", "skills/jira/api_token", "skills/jira/site"]);
  }

  return (
    <IntegrationCard
      icon={Ticket}
      title="Jira"
      description="Your Atlassian API token for Jira access"
      status={status}
      error={vault.error}
    >
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="text-sm font-medium">Atlassian Email</label>
            <Input value={form.email} onChange={(e) => setForm((f) => ({ ...f, email: e.target.value }))} placeholder="you@company.com" />
          </div>
          <div>
            <label className="text-sm font-medium">API Token</label>
            <SecretInput value={form.api_token} onChange={(v) => setForm((f) => ({ ...f, api_token: v }))} placeholder="Atlassian API token" />
          </div>
        </div>
        <div>
          <label className="text-sm font-medium">Site URL</label>
          <Input value={form.site} onChange={(e) => setForm((f) => ({ ...f, site: e.target.value }))} placeholder="https://company.atlassian.net" />
        </div>
        <div className="flex gap-2">
          <Button
            size="sm"
            onClick={handleSave}
            disabled={!form.api_token || vault.saving}
          >
            {vault.saving ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <Save className="h-4 w-4 mr-1" />}
            Save
          </Button>
          {status.scope === "personal" && (
            <Button size="sm" variant="outline" onClick={handleRemove} disabled={vault.removing}>
              <Trash2 className="h-4 w-4 mr-1" />
              Remove
            </Button>
          )}
        </div>
      </div>
    </IntegrationCard>
  );
}

// ── SendGrid Section ────────────────────────────────────────────────────────

const SENDGRID_KEYS = {
  api_key: "skills/sendgrid/api_key",
  from_email: "skills/sendgrid/from_email",
  from_name: "skills/sendgrid/from_name",
} as const;

type SendGridField = keyof typeof SENDGRID_KEYS;

const SENDGRID_ENTRIES = Object.entries(SENDGRID_KEYS) as [SendGridField, string][];

function SendGridSection({ status }: { status: IntegrationStatus }) {
  const [form, setForm] = useState<Record<SendGridField, string>>({ api_key: "", from_email: "", from_name: "" });
  const vault = useIntegrationVault();
  const canSave = status.scope === "personal" ? SENDGRID_ENTRIES.some(([field]) => form[field]) : !!form.api_key;

  function handleSave() {
    vault.save(SENDGRID_ENTRIES.map(([field, path]) => ({ path, value: form[field] })));
  }

  function handleRemove() {
    if (!confirm("Remove your SendGrid configuration?")) return;
    vault.remove(Object.values(SENDGRID_KEYS));
  }

  return (
    <IntegrationCard
      icon={Send}
      title="SendGrid"
      description="Your SendGrid API key and default sender for transactional email"
      status={status}
      error={vault.error}
    >
      <div className="space-y-3">
        <div>
          <label className="text-sm font-medium">API Key</label>
          <SecretInput value={form.api_key} onChange={(v) => setForm((f) => ({ ...f, api_key: v }))} placeholder="SG.…" />
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="text-sm font-medium">From Email</label>
            <Input value={form.from_email} onChange={(e) => setForm((f) => ({ ...f, from_email: e.target.value }))} placeholder="noreply@company.com" />
          </div>
          <div>
            <label className="text-sm font-medium">From Name</label>
            <Input value={form.from_name} onChange={(e) => setForm((f) => ({ ...f, from_name: e.target.value }))} placeholder="Your Name" />
          </div>
        </div>
        <div className="flex gap-2">
          <Button size="sm" onClick={handleSave} disabled={!canSave || vault.saving}>
            {vault.saving ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <Save className="h-4 w-4 mr-1" />}
            Save
          </Button>
          {status.scope === "personal" && (
            <Button size="sm" variant="outline" onClick={handleRemove} disabled={vault.removing}>
              <Trash2 className="h-4 w-4 mr-1" />
              Remove
            </Button>
          )}
        </div>
      </div>
    </IntegrationCard>
  );
}

// ── Speech (TTS) Sections ───────────────────────────────────────────────────

/// One card per TTS provider: the user's own API key (their account gets
/// billed instead of the shared one) and optionally their own voice id.
/// Values are plain secrets at `speech/{provider}/api_key` and `…/voice`.
function SpeechProviderSection({
  provider,
  title,
  description,
  keyPlaceholder,
  voiceLabel,
  voicePlaceholder,
  status,
}: {
  provider: string;
  title: string;
  description: string;
  keyPlaceholder: string;
  voiceLabel: string;
  voicePlaceholder: string;
  status: IntegrationStatus;
}) {
  const [form, setForm] = useState({ api_key: "", voice: "" });
  const vault = useIntegrationVault();

  function handleSave() {
    vault.save([
      { path: `speech/${provider}/api_key`, value: form.api_key },
      { path: `speech/${provider}/voice`, value: form.voice },
    ]);
  }

  function handleRemove() {
    if (!confirm(`Remove your ${title} key and voice?`)) return;
    vault.remove([`speech/${provider}/api_key`, `speech/${provider}/voice`]);
  }

  return (
    <IntegrationCard icon={Mic} title={title} description={description} status={status} error={vault.error}>
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="text-sm font-medium">API Key</label>
            <SecretInput
              value={form.api_key}
              onChange={(v) => setForm((f) => ({ ...f, api_key: v }))}
              placeholder={keyPlaceholder}
            />
          </div>
          <div>
            <label className="text-sm font-medium">{voiceLabel}</label>
            <Input
              value={form.voice}
              onChange={(e) => setForm((f) => ({ ...f, voice: e.target.value }))}
              placeholder={voicePlaceholder}
            />
          </div>
        </div>
        <p className="text-xs text-muted-foreground">
          With your own key, voice clips the agent sends you are billed to your account and
          spoken with your chosen voice. Without one, the shared key (if any) is used.
        </p>
        <div className="flex gap-2">
          <Button
            size="sm"
            onClick={handleSave}
            disabled={(!form.api_key && !form.voice) || vault.saving}
          >
            {vault.saving ? <Loader2 className="h-4 w-4 animate-spin mr-1" /> : <Save className="h-4 w-4 mr-1" />}
            Save
          </Button>
          {status.scope === "personal" && (
            <Button size="sm" variant="outline" onClick={handleRemove} disabled={vault.removing}>
              <Trash2 className="h-4 w-4 mr-1" />
              Remove
            </Button>
          )}
        </div>
      </div>
    </IntegrationCard>
  );
}

// ── Integration Card (shared layout) ────────────────────────────────────────

function useIntegrationVault() {
  const setVault = useSetMyVault();
  const deleteVault = useDeleteMyVault();
  return {
    save: (entries: { path: string; value: string }[]) => {
      deleteVault.reset();
      setVault.mutate(entries.filter(({ value }) => value));
    },
    remove: (paths: string[]) => {
      setVault.reset();
      deleteVault.mutate(paths);
    },
    saving: setVault.isPending,
    removing: deleteVault.isPending,
    error: setVault.error ?? deleteVault.error,
  };
}

type IntegrationCardProps = {
  id?: string;
  icon: typeof Code;
  title: string;
  description: string;
  error: Error | null;
  children: ReactNode;
} & ({ status: IntegrationStatus; badge?: never } | { badge: ReactNode; status?: never });

function IntegrationCard({
  id,
  icon: Icon,
  title,
  description,
  error,
  children,
  ...scope
}: IntegrationCardProps) {
  return (
    <Card id={id} className="scroll-mt-6">
      <CardHeader>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className="rounded-lg bg-muted p-2">
              <Icon className="h-5 w-5 text-muted-foreground" />
            </div>
            <div>
              <CardTitle className="text-base">{title}</CardTitle>
              <CardDescription>{description}</CardDescription>
            </div>
          </div>
          <div className="text-right">
            {scope.status ? (
              <>
                <ScopeBadge scope={scope.status.scope} />
                <p className="text-xs text-muted-foreground mt-1">{scopeDescription(scope.status.scope)}</p>
              </>
            ) : (
              scope.badge
            )}
          </div>
        </div>
      </CardHeader>
      <CardContent>
        {children}
        {error && <p className="mt-3 text-sm text-destructive">{apiErrorMessage(error)}</p>}
      </CardContent>
    </Card>
  );
}

// ── Page ────────────────────────────────────────────────────────────────────

const EMPTY_STATUS: IntegrationStatus = { configured: false, scope: "none" };

export default function MySettingsPage() {
  const { data, isLoading, error } = useMyIntegrations();

  if (isLoading) return <PageSkeleton />;

  if (error) {
    const msg = error instanceof Error ? error.message : String(error);
    const isIdentityRequired = msg.includes("user identity") || msg.includes("static admin token");
    return (
      <div className="p-6 space-y-4">
        <h1 className="text-xl font-bold">My Settings</h1>
        {isIdentityRequired ? (
          <Card>
            <CardContent className="py-8 text-center space-y-2">
              <p className="text-sm text-muted-foreground">
                My Settings requires a personal login.
              </p>
              <p className="text-sm text-muted-foreground">
                Sign in with your email or SSO to configure your personal integrations.
              </p>
            </CardContent>
          </Card>
        ) : (
          <div className="text-destructive text-sm">
            Failed to load settings: {msg}
          </div>
        )}
      </div>
    );
  }

  const integrations = data?.integrations ?? {};
  const identity = data?.identity;

  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-xl font-bold">My Settings</h1>
        <p className="text-sm text-muted-foreground">
          Configure your personal integration credentials.
          {identity?.display_name && (
            <> Logged in as <span className="font-medium">{identity.display_name}</span></>
          )}
          {identity?.email && (
            <> ({identity.email})</>
          )}
        </p>
      </div>

      <div className="space-y-4">
        <SubscriptionSections />
        <GoogleSection status={integrations.google ?? EMPTY_STATUS} />
        <EmailSection status={integrations.email ?? EMPTY_STATUS} />
        <JiraSection status={integrations.jira ?? EMPTY_STATUS} />
        <SendGridSection status={integrations.sendgrid ?? EMPTY_STATUS} />
        <FreeAgentSection status={integrations.freeagent ?? EMPTY_STATUS} />
        <SpeechProviderSection
          provider="fish_audio"
          title="Fish Audio (voice)"
          description="Your Fish Audio key and voice for agent voice clips"
          keyPlaceholder="Fish Audio API key"
          voiceLabel="Voice (model reference id)"
          voicePlaceholder="e.g. 3d9ef257787c40088d95528733f0acfc"
          status={integrations.fish_audio ?? EMPTY_STATUS}
        />
        <SpeechProviderSection
          provider="elevenlabs"
          title="ElevenLabs (voice)"
          description="Your ElevenLabs key and voice for agent voice clips"
          keyPlaceholder="ElevenLabs API key"
          voiceLabel="Voice id"
          voicePlaceholder="e.g. 21m00Tcm4TlvDq8ikWAM"
          status={integrations.elevenlabs ?? EMPTY_STATUS}
        />
        <SpeechProviderSection
          provider="soniox"
          title="Soniox (voice)"
          description="Your Soniox key and voice for agent voice clips"
          keyPlaceholder="Soniox API key"
          voiceLabel="Voice name"
          voicePlaceholder="e.g. Maya"
          status={integrations.soniox ?? EMPTY_STATUS}
        />
      </div>

      <RemoteMcpTokensCard />

      <MyCredentialsSection />
    </div>
  );
}

function MyCredentialsSection() {
  const { data, isLoading } = useMyVault();
  const { data: catalogData } = useVaultCatalog();
  const deleteSecret = useDeleteMySecret();
  const moveVault = useMoveMyVault();
  const [moveTarget, setMoveTarget] = useState<VaultEntryView | null>(null);
  const [showAdd, setShowAdd] = useState(false);
  const entries = data?.entries ?? [];

  const allowedDestinations: VaultTier[] = ["agent_per_user", "global_per_user"];

  async function handleMove(entry: VaultEntryView, to: VaultTier, overwrite: boolean) {
    if (to !== "agent_per_user" && to !== "global_per_user") {
      throw new Error(
        "Only moves between Agent · per user and Global · per user are allowed from this page",
      );
    }
    if (entry.tier !== "agent_per_user" && entry.tier !== "global_per_user") {
      throw new Error("Only per-user entries can be moved from this page");
    }
    await moveVault.mutateAsync({
      path: entry.path,
      from_tier: entry.tier,
      to_tier: to,
      agent_id: entry.agent_id ?? undefined,
      overwrite,
    });
    setMoveTarget(null);
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-3">
          <div>
            <CardTitle className="text-base">All credentials</CardTitle>
            <CardDescription>
              Every credential addressable to you, grouped by tier. Per-user rows
              (Agent · per user / Global · per user) are yours to add, move and
              delete — promote an agent-scoped credential to Global · per user so
              all your agents can reuse it without re-authing. Shared rows (Agent
              specific / Global) are admin-managed and read-only here.
            </CardDescription>
          </div>
          <Button className="shrink-0" onClick={() => setShowAdd(true)}>
            <Plus className="h-4 w-4 mr-1" />
            Add Secret
          </Button>
        </div>
      </CardHeader>
      <CardContent>
        <VaultTable
          entries={entries}
          isLoading={isLoading}
          showOwner={false}
          catalog={catalogData?.entries}
          canMove={(e) =>
            e.tier === "agent_per_user" || e.tier === "global_per_user"
          }
          onMove={setMoveTarget}
          canDelete={(e) =>
            e.tier === "agent_per_user" || e.tier === "global_per_user"
          }
          onDelete={(e) =>
            deleteSecret.mutate({
              path: e.path,
              tier: e.tier as "agent_per_user" | "global_per_user",
              agent_id: e.agent_id,
            })
          }
        />
        {deleteSecret.error && (
          <p className="mt-3 text-sm text-destructive">{apiErrorMessage(deleteSecret.error)}</p>
        )}
        <VaultMoveDialog
          entry={moveTarget}
          allowedDestinations={allowedDestinations}
          pending={moveVault.isPending}
          onOpenChange={(o) => !o && setMoveTarget(null)}
          onMove={handleMove}
        />
        <VaultSecretForm
          open={showAdd}
          onOpenChange={setShowAdd}
          context={{ kind: "member" }}
          mode="add"
        />
      </CardContent>
    </Card>
  );
}
