import type { ChannelDisplay } from "@/lib/api/sessions";

const capitalize = (s: string) => (s ? s.charAt(0).toUpperCase() + s.slice(1) : s);

/** Render a session channel as a human-readable label, with the raw identifier
 *  on hover. Slack channels show `Slack:Workspace name [· thread]`; everything
 *  else shows the server-resolved friendly name (`Web`, `iOS`, …). Falls back
 *  to the raw `channel` string when the server didn't enrich it. */
export function ChannelLabel({
  display,
  channel,
  sessionId,
}: {
  display?: ChannelDisplay;
  channel: string;
  sessionId?: string;
}) {
  if (!display) {
    return (
      <span className="truncate inline-block max-w-[260px] font-mono text-xs" title={channel}>
        {channel}
      </span>
    );
  }

  const title = sessionId ? `${display.raw}\n${sessionId}` : display.raw;

  if (display.platform === "slack") {
    const prefix = display.workspace ? `Slack:${capitalize(display.workspace)}` : "Slack";
    return (
      <span className="truncate inline-block max-w-[260px]" title={title}>
        <span className="text-muted-foreground">{prefix}</span>{" "}
        <span className="font-medium">{display.name}</span>
        {display.thread && <span className="text-muted-foreground"> · thread</span>}
      </span>
    );
  }

  return (
    <span className="truncate inline-block max-w-[260px] font-medium" title={title}>
      {display.name}
    </span>
  );
}
