import {
  keepPreviousData,
  useQuery,
  useMutation,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import { agentsApi, type ConnectSlackRequest } from "@/lib/api/agents";
import { useDebounced } from "@/hooks/use-debounced";

export function useAgents() {
  return useQuery({ queryKey: ["agents"], queryFn: agentsApi.list, staleTime: 30_000 });
}

export function useAgentDetail(id: string | undefined) {
  return useQuery({
    queryKey: ["agent-detail", id],
    queryFn: () => agentsApi.getDetail(id!),
    enabled: !!id,
    staleTime: 15_000,
  });
}

export function useCreateAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: agentsApi.create,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agents"] });
    },
  });
}

export function useUpdateAgent(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: Parameters<typeof agentsApi.update>[1]) => agentsApi.update(id, req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agents"] });
      qc.invalidateQueries({ queryKey: ["agent-detail", id] });
    },
  });
}

export function useDeleteAgent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => agentsApi.delete(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agents"] });
    },
  });
}

export function useAgentIdentity(id: string | undefined, file: string | null) {
  return useQuery({
    queryKey: ["agent-identity", id, file],
    queryFn: () => agentsApi.getIdentity(id!, file!),
    enabled: !!id && !!file,
    staleTime: 10_000,
  });
}

export function useSaveAgentIdentity(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ file, content }: { file: string; content: string }) =>
      agentsApi.putIdentity(id, file, content),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["agent-identity", id, vars.file] });
    },
  });
}

export function useAgentConfigRevisions(id: string | undefined, enabled: boolean) {
  return useQuery({
    queryKey: ["agent-config-revisions", id],
    queryFn: () => agentsApi.listConfigRevisions(id!),
    enabled: !!id && enabled,
    staleTime: 10_000,
  });
}

/** Everything read from an agent's config.toml, refetched once it changes. */
export function invalidateAgentConfig(qc: QueryClient, id: string) {
  for (const queryKey of [
    ["agent-config", id],
    ["agent-config-revisions", id],
    ["agent-logins", id],
    ["agent-environments", id],
  ]) {
    qc.invalidateQueries({ queryKey });
  }
}

export function useRestoreAgentConfigRevision(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (rev: number) => agentsApi.restoreConfigRevision(id, rev),
    onSuccess: () => invalidateAgentConfig(qc, id),
  });
}

export function useIdentityRevisions(id: string | undefined, file: string | null) {
  return useQuery({
    queryKey: ["agent-identity-revisions", id, file],
    queryFn: () => agentsApi.listIdentityRevisions(id!, file!),
    enabled: !!id && !!file,
    staleTime: 10_000,
  });
}

export function useRestoreIdentityRevision(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ file, rev }: { file: string; rev: number }) =>
      agentsApi.restoreIdentityRevision(id, file, rev),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["agent-identity", id, vars.file] });
      qc.invalidateQueries({ queryKey: ["agent-identity-revisions", id, vars.file] });
    },
  });
}

export function useAgentConfig(id: string | undefined) {
  return useQuery({
    queryKey: ["agent-config", id],
    queryFn: () => agentsApi.getConfig(id!),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useSaveAgentConfig(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ toml, etag }: { toml: string; etag: string | null }) =>
      agentsApi.putConfig(id, toml, etag),
    onSuccess: () => invalidateAgentConfig(qc, id),
  });
}

/** What the hub would make of `toml` as the agent's config, checked once the
 *  editing pauses; the last check stays shown meanwhile, and `pending` holds
 *  until the text as it stands has been checked. */
export function useConfigCheck(id: string, toml: string, enabled: boolean) {
  const debounced = useDebounced(toml, 400);
  const settled = debounced === toml;
  const query = useQuery({
    queryKey: ["agent-config-check", id, debounced],
    queryFn: () => agentsApi.checkConfig(id, debounced),
    enabled: enabled && settled,
    placeholderData: keepPreviousData,
    staleTime: Infinity,
    retry: false,
  });
  return {
    data: query.data,
    error: query.error,
    pending: enabled && (!settled || query.isFetching || query.isPlaceholderData),
  };
}

export function useAgentSlack(id: string) {
  return useQuery({
    queryKey: ["agent-slack", id],
    queryFn: () => agentsApi.slackStatus(id),
    staleTime: 10_000,
  });
}

export function useConnectAgentSlack(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: ConnectSlackRequest) => agentsApi.connectSlack(id, req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-slack", id] });
      qc.invalidateQueries({ queryKey: ["agent-tools", id] });
    },
  });
}

export function useDisconnectAgentSlack(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => agentsApi.disconnectSlack(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-slack", id] });
      qc.invalidateQueries({ queryKey: ["agent-tools", id] });
    },
  });
}

export function useAgentTools(id: string | undefined) {
  return useQuery({
    queryKey: ["agent-tools", id],
    queryFn: () => agentsApi.getTools(id!),
    enabled: !!id,
    staleTime: 30_000,
  });
}

export function useAgentStats(id: string | undefined, period = "7d") {
  return useQuery({
    queryKey: ["agent-stats", id, period],
    queryFn: () => agentsApi.getStats(id!, period),
    enabled: !!id,
    staleTime: 60_000,
  });
}

export function useAgentVaultKeys(id: string | undefined) {
  return useQuery({
    queryKey: ["agent-vault", id],
    queryFn: () => agentsApi.listVaultKeys(id!),
    enabled: !!id,
    staleTime: 15_000,
  });
}

export function useAgentVaultSet(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ path, value }: { path: string; value: string }) =>
      agentsApi.setVaultSecret(id, path, value),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-vault", id] });
    },
  });
}

export function useAgentVaultDelete(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (path: string) => agentsApi.deleteVaultSecret(id, path),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-vault", id] });
    },
  });
}

export function useAgentVaultMove(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: Parameters<typeof agentsApi.moveVault>[1]) =>
      agentsApi.moveVault(id, req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-vault", id] });
      qc.invalidateQueries({ queryKey: ["vault"] });
    },
  });
}

export function useAgentVaultCopy(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: Parameters<typeof agentsApi.copyVault>[1]) =>
      agentsApi.copyVault(id, req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-vault", id] });
      qc.invalidateQueries({ queryKey: ["vault"] });
    },
  });
}

export function useAgentSkills(id: string | undefined) {
  return useQuery({
    queryKey: ["agent-skills", id],
    queryFn: () => agentsApi.listSkills(id!),
    enabled: !!id,
    staleTime: 30_000,
  });
}

export function useAgentSkillContent(id: string | undefined, name: string | null) {
  return useQuery({
    queryKey: ["agent-skill", id, name],
    queryFn: () => agentsApi.getSkill(id!, name!),
    enabled: !!id && !!name,
    staleTime: 10_000,
  });
}

export function useSaveAgentSkill(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, content }: { name: string; content: string }) =>
      agentsApi.putSkill(id, name, content),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["agent-skill", id, vars.name] });
      qc.invalidateQueries({ queryKey: ["agent-skills", id] });
    },
  });
}

export function useDeleteAgentSkill(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => agentsApi.deleteSkill(id, name),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-skills", id] });
    },
  });
}

export function useAgentSkillConfig(id: string | undefined, name: string | null) {
  return useQuery({
    queryKey: ["agent-skill-config", id, name],
    queryFn: () => agentsApi.getSkillConfig(id!, name!),
    enabled: !!id && !!name,
    staleTime: 10_000,
  });
}

export function useSaveAgentSkillConfig(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ name, content }: { name: string; content: string }) =>
      agentsApi.putSkillConfig(id, name, content),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["agent-skill-config", id, vars.name] });
    },
  });
}
