import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { configApi } from "@/lib/api/config";

export function useConfig() {
  return useQuery({
    queryKey: ["workforce-config"],
    queryFn: configApi.get,
    staleTime: 15_000,
  });
}

export function useSaveConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (toml: string) => configApi.put(toml),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workforce-config"] });
    },
  });
}

export function useConfigSchema() {
  return useQuery({
    queryKey: ["workforce-config-schema"],
    queryFn: configApi.schema,
    staleTime: 300_000,
  });
}

export function useRawConfig() {
  return useQuery({
    queryKey: ["workforce-config-raw"],
    queryFn: configApi.getRaw,
    staleTime: 15_000,
  });
}

export function useSaveRawConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (toml: string) => configApi.putRaw(toml),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workforce-config-raw"] });
      qc.invalidateQueries({ queryKey: ["workforce-config"] });
    },
  });
}

export function useEffectiveConfig() {
  return useQuery({
    queryKey: ["workforce-config-effective"],
    queryFn: configApi.getEffective,
    staleTime: 15_000,
  });
}

export function useConfigOverlay() {
  return useQuery({
    queryKey: ["workforce-config-overlay"],
    queryFn: configApi.getOverlay,
    staleTime: 15_000,
  });
}

function invalidateOverlayQueries(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ["workforce-config-effective"] });
  qc.invalidateQueries({ queryKey: ["workforce-config-overlay"] });
  qc.invalidateQueries({ queryKey: ["workforce-config"] });
}

export function useSaveOverlay() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (toml: string) => configApi.putOverlay(toml),
    onSuccess: () => invalidateOverlayQueries(qc),
  });
}

export function useRevertOverlayKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (keyPath: string) => configApi.revertOverlayKey(keyPath),
    onSuccess: () => invalidateOverlayQueries(qc),
  });
}

export function useClearOverlay() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => configApi.clearOverlay(),
    onSuccess: () => invalidateOverlayQueries(qc),
  });
}
