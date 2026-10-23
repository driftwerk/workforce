import { useMemo } from "react";
import { useAgents } from "@/hooks/use-agents";

export interface AgentOption {
  value: string;
  label: string;
}

// Fallback when the live agents list is empty or errors. These are the agents
// the Lessons feature ships against.
const STATIC_AGENTS = ["example", "quill", "candy", "bernard", "lynx"];

const ALL_OPTION: AgentOption = { value: "all", label: "All agents" };

/** The agent options for the Lessons selectors — "All" first, then the live
 *  agent list (falling back to a static set). */
export function useAgentOptions(): AgentOption[] {
  const { data } = useAgents();
  return useMemo(() => {
    const live = data?.agents ?? [];
    const source =
      live.length > 0
        ? live.map((a) => ({ value: a.id, label: a.name || a.id }))
        : STATIC_AGENTS.map((id) => ({ value: id, label: id }));
    return [ALL_OPTION, ...source];
  }, [data]);
}

interface Props {
  value: string;
  onChange: (value: string) => void;
  options: AgentOption[];
}

export function AgentSelect({ value, onChange, options }: Props) {
  return (
    <select
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className="h-9 rounded-md border border-input bg-background px-3 py-1.5 text-sm ring-offset-background focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}
