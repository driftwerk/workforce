import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  vaultApi,
  type CatalogResponse,
  type VaultListResponse,
} from "@/lib/api/vault";

export type {
  VaultEntryView,
  VaultTier,
  VaultMoveRequest,
  CatalogEntry,
  CatalogVariable,
} from "@/lib/api/vault";
export { VAULT_TIERS } from "@/lib/api/vault";

/** Credential catalog driving the vault editor's path dropdown — the static
 * skill/preset catalog merged with the installed MCP servers' credentials
 * (preset + custom). The MCP fetch is admin-only; when it's unavailable (e.g.
 * the non-admin My Settings view) we fall back to the static catalog. */
export function useVaultCatalog() {
  return useQuery<CatalogResponse>({
    queryKey: ["vault-catalog"],
    queryFn: async () => {
      const base = await vaultApi.catalog();
      const installed = await vaultApi.mcpCatalog().catch(() => null);
      if (!installed) return base;
      const seen = new Set(base.entries.map((e) => e.template));
      const extra = installed.entries.filter((e) => !seen.has(e.template));
      return { entries: [...base.entries, ...extra] };
    },
    staleTime: 5 * 60_000,
  });
}

/**
 * Rename a secret within the GLOBAL vault in one server-side write. The paths
 * are physical (already `users/{uid}/`-prefixed for per-user tiers).
 */
export function useVaultRename() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ oldPath, newPath }: { oldPath: string; newPath: string }) =>
      vaultApi.rename(oldPath, newPath),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["vault"] }),
  });
}

export function useVault(prefix?: string) {
  return useQuery<VaultListResponse>({
    queryKey: ["vault", prefix],
    queryFn: () => vaultApi.list(prefix),
    staleTime: 15_000,
  });
}

export function useVaultSet() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ path, value }: { path: string; value: string }) =>
      vaultApi.set(path, value),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["vault"] });
    },
  });
}

export function useVaultDelete() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (path: string) => vaultApi.del(path),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["vault"] });
    },
  });
}
