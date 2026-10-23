import { useCallback, useState } from "react";

export type SessionViewMode = "flat" | "user" | "agent";

/** Sessions page view preferences, persisted across navigation so the chosen
 *  grouping (and which groups are expanded) survives a reload. */
export interface SessionsViewPrefs {
  viewMode: SessionViewMode;
  /** In the "by agent" view, also sub-group each agent's sessions by user. */
  agentSubgroupByUser: boolean;
  /** Group keys the user has collapsed. Absent = expanded (default). */
  collapsed: Record<string, boolean>;
}

const KEY = "workforce-sessions-view-prefs";
const DEFAULTS: SessionsViewPrefs = {
  viewMode: "flat",
  agentSubgroupByUser: false,
  collapsed: {},
};

const MODES: SessionViewMode[] = ["flat", "user", "agent"];

function load(): SessionsViewPrefs {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return DEFAULTS;
    const p = JSON.parse(raw) as Partial<SessionsViewPrefs>;
    return {
      viewMode: MODES.includes(p.viewMode as SessionViewMode)
        ? (p.viewMode as SessionViewMode)
        : "flat",
      agentSubgroupByUser: p.agentSubgroupByUser === true,
      collapsed:
        p.collapsed && typeof p.collapsed === "object" ? p.collapsed : {},
    };
  } catch {
    return DEFAULTS;
  }
}

export function useSessionsViewPrefs(): {
  prefs: SessionsViewPrefs;
  update: (patch: Partial<SessionsViewPrefs>) => void;
  toggleCollapsed: (key: string) => void;
} {
  const [prefs, setPrefs] = useState<SessionsViewPrefs>(load);

  const persist = useCallback((next: SessionsViewPrefs) => {
    try {
      localStorage.setItem(KEY, JSON.stringify(next));
    } catch {
      // Storage unavailable (private mode / quota) — keep in-memory state.
    }
    return next;
  }, []);

  const update = useCallback(
    (patch: Partial<SessionsViewPrefs>) =>
      setPrefs((prev) => persist({ ...prev, ...patch })),
    [persist],
  );

  // Only collapsed (true) keys are kept, so the map doesn't grow unbounded as
  // groups are expanded/collapsed or as users/agents come and go.
  const toggleCollapsed = useCallback(
    (key: string) =>
      setPrefs((prev) => {
        const collapsed = { ...prev.collapsed };
        if (collapsed[key]) delete collapsed[key];
        else collapsed[key] = true;
        return persist({ ...prev, collapsed });
      }),
    [persist],
  );

  return { prefs, update, toggleCollapsed };
}
