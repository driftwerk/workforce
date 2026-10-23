import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { useAgentSettings } from "@/hooks/use-agent-settings";
import type { SettingRow, SettingSource, SettingStatus } from "@/hooks/use-agent-settings";
import { isNotable } from "@/lib/api/agent-settings";
import { apiErrorMessage } from "@/lib/api-error";

const SOURCE_LABELS: Record<SettingSource, string> = {
  default: "Default",
  install: "Install",
  install_agent: "Install (agent entry)",
  agent: "Agent config",
  runtime: "Runtime",
  environment: "Environment",
  vault: "Vault",
};

const STATUS_LABELS: Record<SettingStatus, string> = {
  active: "Active",
  off: "Off",
  kill_switched: "Off by install",
  shadow: "Shadow",
  inert: "Inert",
};

const STATUS_VARIANTS: Record<
  SettingStatus,
  "default" | "secondary" | "outline" | "success" | "warning"
> = {
  active: "success",
  off: "secondary",
  kill_switched: "outline",
  shadow: "default",
  inert: "warning",
};

const shown = (value: unknown): string =>
  typeof value === "string" ? value : JSON.stringify(value);

const matches = (row: SettingRow, needle: string): boolean =>
  [row.section, row.key, shown(row.value), row.reason ?? ""].some((text) =>
    text.toLowerCase().includes(needle),
  );

interface AgentSettingsProps {
  agentId: string;
}

export function AgentSettings({ agentId }: AgentSettingsProps) {
  const { data, isLoading, error } = useAgentSettings(agentId);
  const [filter, setFilter] = useState("");
  const [changedOnly, setChangedOnly] = useState(true);

  if (isLoading) {
    return <div className="py-12 text-center text-sm text-muted-foreground">Loading…</div>;
  }
  if (error || !data) {
    return (
      <div className="rounded-md border border-destructive/30 bg-destructive/10 p-3 text-sm text-destructive">
        {error ? apiErrorMessage(error) : "No settings were returned."}
      </div>
    );
  }

  const needle = filter.trim().toLowerCase();
  const rows = data.rows.filter(
    (row) => (!changedOnly || isNotable(row)) && (!needle || matches(row, needle)),
  );
  const sections = data.sections
    .map((section) => ({ section, rows: rows.filter((row) => row.section === section) }))
    .filter(({ rows }) => rows.length > 0);

  return (
    <div className="space-y-4">
      <div>
        <h3 className="text-lg font-semibold">Effective settings</h3>
        <p className="text-sm text-muted-foreground">
          What this agent runs with and where each value comes from. Read-only: change values in
          the Configuration tab or the install config. Keys and tokens show only whether they are
          set.{" "}
          {data.viewer === "admin"
            ? "Install-wide settings are included."
            : "Install-wide settings are shown to admins."}
        </p>
      </div>

      <div className="flex items-center gap-4">
        <Input
          className="max-w-xs"
          placeholder="Filter settings…"
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
        />
        <label className="flex items-center gap-2 text-sm text-muted-foreground">
          <input
            type="checkbox"
            checked={changedOnly}
            onChange={(event) => setChangedOnly(event.target.checked)}
          />
          Hide built-in defaults
        </label>
      </div>

      {sections.length === 0 && (
        <div className="py-8 text-center text-sm text-muted-foreground">No settings match.</div>
      )}

      {sections.map(({ section, rows }) => (
        <div key={section} className="border rounded-lg overflow-hidden">
          <div className="border-b bg-muted/50 px-4 py-2 font-mono text-sm font-medium">
            {section}
          </div>
          <table className="w-full">
            <tbody>
              {rows.map((row) => (
                <tr key={row.key} className="border-b last:border-b-0 align-top">
                  <td className="px-4 py-2 w-64 font-mono text-xs break-all">{row.key}</td>
                  <td className="px-4 py-2 font-mono text-xs break-all">
                    {shown(row.value)}
                    {row.reason && (
                      <div className="mt-1 font-sans text-xs text-muted-foreground">
                        {row.reason}
                      </div>
                    )}
                  </td>
                  <td
                    className="px-4 py-2 w-40 text-xs text-muted-foreground"
                    title={row.chain
                      ?.map((layer) => `${SOURCE_LABELS[layer.source]}: ${shown(layer.value)}`)
                      .join("\n")}
                  >
                    {row.source && SOURCE_LABELS[row.source]}
                  </td>
                  <td className="px-4 py-2 w-32">
                    {row.status && (
                      <Badge variant={STATUS_VARIANTS[row.status]}>
                        {STATUS_LABELS[row.status]}
                      </Badge>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ))}
    </div>
  );
}
