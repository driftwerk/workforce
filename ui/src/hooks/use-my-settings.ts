import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import {
  mySettingsApi,
  type RemoteMcpToken,
  type SetSecretRequest,
  type DeleteSecretRequest,
} from "@/lib/api/my-settings";
import { ApiError } from "@/lib/api-client";

export function useMyIntegrations() {
  return useQuery({
    queryKey: ["my-integrations"],
    queryFn: mySettingsApi.getIntegrations,
    staleTime: 10_000,
  });
}

export function useMyVault() {
  return useQuery({
    queryKey: ["my-vault"],
    queryFn: mySettingsApi.listVault,
    staleTime: 10_000,
  });
}

export function useSetMyVault() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (entries: { path: string; value: string }[]) => {
      for (const { path, value } of entries) {
        await mySettingsApi.setVault(path, value);
      }
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["my-integrations"] });
      qc.invalidateQueries({ queryKey: ["my-vault"] });
    },
  });
}

export function useSetMySecret() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: SetSecretRequest) => mySettingsApi.setSecret(req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["my-vault"] });
      qc.invalidateQueries({ queryKey: ["my-integrations"] });
    },
  });
}

export function useDeleteMyVault() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (paths: string[]) => {
      for (const path of paths) {
        await mySettingsApi.deleteVault(path);
      }
    },
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["my-integrations"] });
      qc.invalidateQueries({ queryKey: ["my-vault"] });
    },
  });
}

export function useDeleteMySecret() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: DeleteSecretRequest) => mySettingsApi.deleteSecret(req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["my-vault"] });
      qc.invalidateQueries({ queryKey: ["my-integrations"] });
    },
  });
}

export function useMoveMyVault() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: Parameters<typeof mySettingsApi.moveVault>[0]) =>
      mySettingsApi.moveVault(req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["my-integrations"] });
      qc.invalidateQueries({ queryKey: ["my-vault"] });
    },
  });
}

export type RemoteMcpTokensView =
  | { enabled: false }
  | { enabled: true; tokens: RemoteMcpToken[] };

export function useRemoteMcpTokens() {
  return useQuery<RemoteMcpTokensView>({
    queryKey: ["my-remote-mcp-tokens"],
    queryFn: async () => {
      try {
        const res = await mySettingsApi.listRemoteMcpTokens();
        return { enabled: true, tokens: res.tokens };
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) return { enabled: false };
        throw e;
      }
    },
    staleTime: 10_000,
  });
}

export function useCreateRemoteMcpToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (label: string) => mySettingsApi.createRemoteMcpToken(label),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["my-remote-mcp-tokens"] }),
  });
}

export function useRevokeRemoteMcpToken() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => mySettingsApi.revokeRemoteMcpToken(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["my-remote-mcp-tokens"] }),
  });
}
