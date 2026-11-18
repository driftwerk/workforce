import { Link } from "react-router";
import { Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";
import { apiErrorMessage } from "@/lib/api-error";
import type { PersonalRouteKind, RouteFallback, RouteOption } from "@/lib/api/llm-route";

/** What a person can run their work on: the hub's models, one of their own
 *  subscriptions, or (on one agent) whatever their own settings say. */
export type RouteChoice = "settings" | "hub" | PersonalRouteKind;

export const ROUTE_LABELS: Record<PersonalRouteKind, string> = {
  "claude-code": "My Claude",
  codex: "My ChatGPT (Codex)",
  devin: "My Devin",
};

const PROVIDERS: Record<PersonalRouteKind, string> = {
  "claude-code": "Anthropic, under your own Claude account",
  codex: "OpenAI, under your own ChatGPT account",
  devin: "Cognition, under your own Devin account",
};

/** Where the sign-in card for a kind's login is in My Settings. */
const LOGIN_CARDS: Partial<Record<PersonalRouteKind, string>> = {
  "claude-code": "claude",
  codex: "chatgpt",
};

/** The line saying where a person's work goes on `kind`; `null` is the hub. */
export function whereDataGoes(kind: PersonalRouteKind | null): string {
  if (!kind) return "Your conversations stay on the hub's own models, as everyone else's do.";
  return `Your conversations, and the tool results in them, go to ${PROVIDERS[kind]}.`;
}

export const HUB_CHOICE = { value: "hub" as const, label: "Hub's models", refused: null };

export interface ChoiceRow {
  value: RouteChoice;
  label: string;
  detail?: string;
  refused: string | null;
}

/** One row per kind, as the hub offers it here. */
export function kindRows(options: RouteOption[]): ChoiceRow[] {
  return options.map((option) => ({
    value: option.kind,
    label: ROUTE_LABELS[option.kind],
    refused: option.refused,
  }));
}

function Refusal({ value, reason }: { value: RouteChoice; reason: string }) {
  const card = value !== "settings" && value !== "hub" ? LOGIN_CARDS[value] : undefined;
  const connect = card && reason.toLowerCase().startsWith("connect");
  return (
    <span className="block text-xs text-muted-foreground">
      {reason.charAt(0).toUpperCase() + reason.slice(1)}
      {connect && (
        <>
          {" · "}
          <Link to={`/my-settings#${card}`} className="underline">
            Connect
          </Link>
        </>
      )}
    </span>
  );
}

export function RoutePicker({
  name,
  rows,
  value,
  fallback,
  showFallback,
  dataLine,
  pending,
  error,
  onChange,
}: {
  name: string;
  rows: ChoiceRow[];
  value: RouteChoice;
  fallback: RouteFallback;
  /** Whether the chosen row is a person's own route, so the fallback line applies. */
  showFallback: boolean;
  dataLine: string;
  pending: boolean;
  error: unknown;
  onChange: (choice: RouteChoice, fallback: RouteFallback) => void;
}) {
  return (
    <div className="space-y-4">
      <fieldset className="space-y-2" disabled={pending}>
        <legend className="mb-2 text-sm font-medium">Run my work on</legend>
        {rows.map((row) => {
          // A refused kind can't be picked, but one already chosen stays
          // shown as chosen, with why it no longer serves.
          const refused = row.refused && row.value !== value ? row.refused : null;
          return (
            <label
              key={row.value}
              className={cn(
                "flex items-start gap-3 rounded-md border p-3 text-sm",
                refused ? "cursor-not-allowed opacity-60" : "cursor-pointer hover:bg-muted/50",
                row.value === value && "border-primary",
              )}
            >
              <input
                type="radio"
                name={name}
                className="mt-0.5"
                checked={row.value === value}
                disabled={!!refused}
                onChange={() => onChange(row.value, fallback)}
              />
              <span>
                <span className="font-medium">{row.label}</span>
                {row.detail && <span className="block text-xs text-muted-foreground">{row.detail}</span>}
                {row.refused && <Refusal value={row.value} reason={row.refused} />}
              </span>
            </label>
          );
        })}
      </fieldset>

      {showFallback && (
        <fieldset className="space-y-2" disabled={pending}>
          <legend className="mb-2 text-sm font-medium">When my plan is used up or my login fails</legend>
          <div className="flex flex-wrap gap-x-6 gap-y-2">
            {(
              [
                ["wait", "Wait until it's back, and say so"],
                ["hub", "Run that turn on the hub's models, and say so"],
              ] as const
            ).map(([option, label]) => (
              <label key={option} className="flex items-center gap-2 text-sm">
                <input
                  type="radio"
                  name={`${name}-fallback`}
                  checked={fallback === option}
                  onChange={() => onChange(value, option)}
                />
                {label}
              </label>
            ))}
          </div>
        </fieldset>
      )}

      <p className="text-xs text-muted-foreground">{dataLine}</p>
      {pending && (
        <p className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="h-3 w-3 animate-spin" /> Saving…
        </p>
      )}
      {error != null && <p className="text-sm text-destructive">{apiErrorMessage(error)}</p>}
    </div>
  );
}
