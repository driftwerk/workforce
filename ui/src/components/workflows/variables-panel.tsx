import { Plus, Trash2, KeyRound } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card } from "@/components/ui/card";
import type { VariableDef } from "@/lib/api/workflows";
import {
  buildSecretMarker,
  parseSecretMarker,
  VAULT_SCOPES,
} from "@/lib/workflows/variables-toml";

interface VariablesPanelProps {
  variables: VariableDef[];
  onChange: (variables: VariableDef[]) => void;
}

const selectClass =
  "h-9 rounded-md border border-input bg-background px-2 text-xs focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring";

/**
 * Structured editor for a workflow's `[[variables]]` block. Edits flow back as
 * the full variables array; the page splices them into the TOML losslessly.
 */
export function VariablesPanel({ variables, onChange }: VariablesPanelProps) {
  const update = (i: number, patch: Partial<VariableDef>) =>
    onChange(variables.map((v, j) => (j === i ? { ...v, ...patch } : v)));

  const remove = (i: number) => onChange(variables.filter((_, j) => j !== i));

  const add = () => onChange([...variables, { id: "", value: "" }]);

  const addSecret = () =>
    onChange([
      ...variables,
      { id: "", type: "secret", value: buildSecretMarker("global_user", ""), allowed_hosts: [] },
    ]);

  return (
    <div className="mx-auto max-w-3xl space-y-3 p-4">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold">Variables</h2>
          <p className="text-xs text-muted-foreground">
            Typed inputs with a default. A <span className="font-medium">secret</span> resolves from
            the vault at run time and never reaches the model. Use{" "}
            <code className="text-[11px]">global_user</code> for a secret you set yourself.
          </p>
        </div>
        <div className="flex gap-2 shrink-0">
          <Button variant="outline" size="sm" className="h-7 text-xs" onClick={add}>
            <Plus className="mr-1 h-3.5 w-3.5" />
            String
          </Button>
          <Button variant="outline" size="sm" className="h-7 text-xs" onClick={addSecret}>
            <KeyRound className="mr-1 h-3.5 w-3.5" />
            Secret
          </Button>
        </div>
      </div>

      {variables.length === 0 && (
        <div className="rounded-md border border-dashed py-8 text-center text-xs text-muted-foreground">
          No variables. Add a string default or a vault-backed secret.
        </div>
      )}

      {variables.map((v, i) => {
        const isSecret = v.type === "secret";
        const marker = parseSecretMarker(v.value);
        const scope = marker?.scope ?? "global_user";
        const key = marker?.key ?? "";
        return (
          <Card key={i} className="space-y-2 p-3">
            <div className="flex items-center gap-2">
              <Input
                value={v.id}
                onChange={(e) => update(i, { id: e.target.value })}
                placeholder="variable id"
                className="h-9 max-w-[14rem] font-mono text-xs"
              />
              <select
                className={selectClass}
                value={isSecret ? "secret" : "string"}
                onChange={(e) => {
                  const nextSecret = e.target.value === "secret";
                  update(i, {
                    type: nextSecret ? "secret" : "string",
                    value: nextSecret ? buildSecretMarker("global_user", "") : "",
                    allowed_hosts: nextSecret ? (v.allowed_hosts ?? []) : undefined,
                  });
                }}
              >
                <option value="string">string</option>
                <option value="secret">secret</option>
              </select>
              <div className="flex-1" />
              <Button
                variant="ghost"
                size="icon"
                className="h-8 w-8 text-destructive hover:text-destructive"
                onClick={() => remove(i)}
              >
                <Trash2 className="h-3.5 w-3.5" />
              </Button>
            </div>

            {isSecret ? (
              <div className="space-y-2 pl-1">
                <div className="flex items-center gap-2">
                  <span className="text-[11px] text-muted-foreground w-12">vault</span>
                  <select
                    className={selectClass}
                    value={scope}
                    onChange={(e) => update(i, { value: buildSecretMarker(e.target.value, key) })}
                  >
                    {VAULT_SCOPES.map((s) => (
                      <option key={s} value={s}>
                        {s}
                      </option>
                    ))}
                  </select>
                  <Input
                    value={key}
                    onChange={(e) => update(i, { value: buildSecretMarker(scope, e.target.value) })}
                    placeholder="vault key (e.g. portal_password)"
                    className="h-9 flex-1 font-mono text-xs"
                  />
                </div>
                <div className="flex items-center gap-2">
                  <span className="text-[11px] text-muted-foreground w-12">hosts</span>
                  <Input
                    value={(v.allowed_hosts ?? []).join(", ")}
                    onChange={(e) =>
                      update(i, {
                        allowed_hosts: e.target.value
                          .split(",")
                          .map((h) => h.trim())
                          .filter(Boolean),
                      })
                    }
                    placeholder="allowed egress hosts (comma-separated, optional)"
                    className="h-9 flex-1 text-xs"
                  />
                </div>
                <p className="pl-14 font-mono text-[10px] text-muted-foreground">{v.value}</p>
              </div>
            ) : (
              <div className="flex items-center gap-2 pl-1">
                <span className="text-[11px] text-muted-foreground w-12">value</span>
                <Input
                  value={v.value}
                  onChange={(e) => update(i, { value: e.target.value })}
                  placeholder="default value"
                  className="h-9 flex-1 text-xs"
                />
              </div>
            )}
          </Card>
        );
      })}
    </div>
  );
}
