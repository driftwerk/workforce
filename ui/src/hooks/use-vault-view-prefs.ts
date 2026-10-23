import { useCallback, useState } from "react";
import { VAULT_TIERS, type VaultTier } from "@/lib/api/vault";

export type VaultViewMode = "grouped" | "flat";

/** Vault list preferences persisted across navigation (chips + view mode).
 * The free-text search box is deliberately excluded — a stale search carried
 * between views is hostile. */
export interface VaultViewPrefs {
  viewMode: VaultViewMode;
  tierFilter: VaultTier | "all";
  onlyOrphans: boolean;
}

const KEY = "workforce-vault-view-prefs";
const DEFAULTS: VaultViewPrefs = {
  viewMode: "grouped",
  tierFilter: "all",
  onlyOrphans: false,
};

function load(): VaultViewPrefs {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return DEFAULTS;
    const p = JSON.parse(raw) as Partial<VaultViewPrefs>;
    const tier =
      p.tierFilter === "all" ||
      (typeof p.tierFilter === "string" &&
        (VAULT_TIERS as string[]).includes(p.tierFilter))
        ? (p.tierFilter as VaultTier | "all")
        : "all";
    return {
      viewMode: p.viewMode === "flat" ? "flat" : "grouped",
      tierFilter: tier,
      onlyOrphans: p.onlyOrphans === true,
    };
  } catch {
    return DEFAULTS;
  }
}

/** Read/update the persisted vault view preferences. Every `VaultTable` shares
 * the same localStorage key, so a choice made in one surface is remembered in
 * the others. */
export function useVaultViewPrefs(): [
  VaultViewPrefs,
  (patch: Partial<VaultViewPrefs>) => void,
] {
  const [prefs, setPrefs] = useState<VaultViewPrefs>(load);
  const update = useCallback((patch: Partial<VaultViewPrefs>) => {
    setPrefs((prev) => {
      const next = { ...prev, ...patch };
      try {
        localStorage.setItem(KEY, JSON.stringify(next));
      } catch {
        // Storage unavailable (private mode / quota) — keep in-memory state.
      }
      return next;
    });
  }, []);
  return [prefs, update];
}
