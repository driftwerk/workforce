import { memo } from "react";
import { Handle, Position, type NodeProps } from "@xyflow/react";
import type { TerminalNodeData } from "@/lib/playbooks/toml-parser";

/** A synthetic node a button edge terminates at: the playbook's end (`finish` /
 *  `exit`) or a red marker for a `goto` whose target step doesn't exist. */
function TerminalNodeBase({ data }: NodeProps) {
  const { kind, label } = data as unknown as TerminalNodeData;
  const missing = kind === "missing";
  const icon = kind === "finish" ? "✅" : kind === "exit" ? "✕" : "⚠";
  const cls = missing
    ? "border-red-500 text-red-500 bg-red-500/10"
    : kind === "finish"
      ? "border-emerald-600/60 text-emerald-700 dark:text-emerald-400 bg-emerald-600/10"
      : "border-border text-muted-foreground bg-muted/50";

  return (
    <>
      <Handle type="target" position={Position.Top} className="!bg-muted-foreground !w-2 !h-2" />
      <div className={`rounded-full border-2 px-3 py-1.5 text-xs font-medium shadow-sm ${cls}`}>
        <span className="mr-1">{icon}</span>
        {missing ? `missing: ${label}` : label}
      </div>
    </>
  );
}

export const TerminalNode = memo(TerminalNodeBase);
