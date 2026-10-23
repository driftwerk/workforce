import { gateway } from "../api-client";

/** One key path the overlay sets, with converged-with-base detection. */
export interface OverlayKey {
  key_path: string;
  /** The base now carries an identical value — the override is redundant
   *  and can be reverted without changing effective behaviour. */
  converged: boolean;
}

export interface EffectiveConfig {
  /** Merged (base ⊕ overlay) raw TOML — ${VAR}/vault refs unexpanded. */
  effective_toml: string;
  overlay_present: boolean;
  overlay_keys: OverlayKey[];
  /** Non-allowlisted keys found (and ignored) in a hand-edited overlay. */
  stripped: string[];
  overlay_error: string | null;
}

export const configApi = {
  get: () => gateway.get<string>("/api/config"),
  put: (toml: string) => gateway.putText("/api/config", toml),
  schema: () => gateway.get<Record<string, unknown>>("/api/config/schema"),
  getRaw: () => gateway.get<string>("/api/config/raw"),
  putRaw: (toml: string) => gateway.putText<{ saved: boolean }>("/api/config/raw", toml),
  // Config overlay
  getEffective: () => gateway.get<EffectiveConfig>("/api/config/effective"),
  getOverlay: () => gateway.get<string>("/api/config/overlay"),
  putOverlay: (toml: string) =>
    gateway.putText<{ saved: boolean; restart_required: boolean }>(
      "/api/config/overlay",
      toml,
    ),
  exportConfig: (mode: "full" | "fragment") =>
    gateway.get<string>(`/api/config/export?mode=${mode}`),
  revertOverlayKey: (key_path: string) =>
    gateway.post<{ reverted: string; restart_required: boolean }>(
      "/api/config/overlay/revert",
      { key_path },
    ),
  clearOverlay: () =>
    gateway.post<{ cleared: boolean; restart_required: boolean }>(
      "/api/config/overlay/clear",
      {},
    ),
};
