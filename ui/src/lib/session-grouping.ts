import type { Session } from "@/lib/api/sessions";

/** A bucket of sessions for the grouped Sessions views. Either a leaf (with
 *  `sessions`) or a parent (with `subgroups`). */
export interface SessionGroup {
  key: string;
  title: string;
  /** Secondary, dimmed text after the title (e.g. a raw id). */
  subtitle?: string;
  count: number;
  sessions: Session[];
  subgroups?: SessionGroup[];
}

/** Coarse conversation type used for sub-grouping. Branches on the server's
 *  structured `kind` rather than sniffing the display name. */
export function chatType(s: Session): string {
  const d = s.channel_display;
  if (!d) return "Other";
  if (d.platform === "slack") {
    if (d.kind === "dm") return "Slack DM";
    if (d.kind === "group") return "Slack group";
    return "Slack channel";
  }
  return d.name;
}

const newest = (sessions: Session[]): number =>
  sessions.reduce((max, s) => Math.max(max, new Date(s.updated_at).getTime()), 0);

/** Group by an arbitrary key, newest-activity first within and between groups
 *  (matching the flat view's ordering). */
function bucket(
  sessions: Session[],
  keyFn: (s: Session) => string,
): Array<{ key: string; sessions: Session[] }> {
  const map = new Map<string, Session[]>();
  for (const s of sessions) {
    const k = keyFn(s);
    const list = map.get(k) ?? [];
    list.push(s);
    map.set(k, list);
  }
  return [...map.entries()]
    .map(([key, list]) => ({ key, sessions: list }))
    .sort((a, b) => newest(b.sessions) - newest(a.sessions));
}

/** One group per user, sub-grouped by conversation type. */
export function groupByUser(sessions: Session[]): SessionGroup[] {
  return bucket(sessions, (s) => s.owner).map(({ key: owner, sessions: list }) => ({
    key: `user:${owner}`,
    title: list[0]?.owner_display_name || owner,
    subtitle: list[0]?.owner_display_name && list[0].owner_display_name !== owner ? owner : undefined,
    count: list.length,
    sessions: [],
    subgroups: bucket(list, chatType).map(({ key: type, sessions: typeList }) => ({
      key: `user:${owner}/${type}`,
      title: type,
      count: typeList.length,
      sessions: typeList,
    })),
  }));
}

/** One group per agent; optionally sub-grouped by user. */
export function groupByAgent(sessions: Session[], subgroupByUser: boolean): SessionGroup[] {
  return bucket(sessions, (s) => s.agent_id).map(({ key: agentId, sessions: list }) => {
    const base: SessionGroup = {
      key: `agent:${agentId}`,
      title: list[0]?.agent_display_name || agentId,
      count: list.length,
      sessions: subgroupByUser ? [] : list,
    };
    if (!subgroupByUser) return base;
    return {
      ...base,
      subgroups: bucket(list, (s) => s.owner).map(({ key: owner, sessions: userList }) => ({
        key: `agent:${agentId}/${owner}`,
        title: userList[0]?.owner_display_name || owner,
        count: userList.length,
        sessions: userList,
      })),
    };
  });
}
