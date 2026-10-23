import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { workflowsApi, type RunAs } from "@/lib/api/workflows";

export function useWorkflows() {
  return useQuery({ queryKey: ["workflows"], queryFn: workflowsApi.list, staleTime: 30_000 });
}

export function useWorkflowSource(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["workflow-source", id, owner ?? null],
    queryFn: () => workflowsApi.getSource(id!, owner),
    enabled: !!id,
    staleTime: 60_000,
  });
}

export function useSaveWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      toml,
      from,
      visibility,
      owner,
      runAs,
    }: {
      id: string;
      toml: string;
      from?: string;
      visibility?: string;
      owner?: string | null;
      runAs?: RunAs;
    }) => workflowsApi.save(id, toml, from, visibility, owner, runAs),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["workflows"] });
      qc.invalidateQueries({ queryKey: ["workflow-source", vars.id] });
      if (vars.from && vars.from !== vars.id) {
        qc.invalidateQueries({ queryKey: ["workflow-source", vars.from] });
      }
    },
  });
}

export function useWorkflowRevisions(id: string | undefined, owner?: string | null) {
  return useQuery({
    queryKey: ["workflow-revisions", id, owner ?? null],
    queryFn: () => workflowsApi.listRevisions(id!, owner),
    enabled: !!id,
    staleTime: 10_000,
  });
}

export function useRestoreWorkflowRevision() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, rev, owner }: { id: string; rev: number; owner?: string | null }) =>
      workflowsApi.restoreRevision(id, rev, owner),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["workflows"] });
      qc.invalidateQueries({ queryKey: ["workflow-source", vars.id] });
      qc.invalidateQueries({ queryKey: ["workflow-revisions", vars.id] });
    },
  });
}

export function useDeleteWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, owner }: { id: string; owner?: string | null }) =>
      workflowsApi.del(id, owner),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workflows"] });
    },
  });
}

export function useWorkflowInstances(status?: string) {
  return useQuery({ queryKey: ["workflow-instances", status], queryFn: () => workflowsApi.listInstances(status), staleTime: 10_000 });
}

export function useWorkflowInstancesForDef(workflowId: string | undefined) {
  return useQuery({
    queryKey: ["workflow-instances", "by-def", workflowId],
    queryFn: () => workflowsApi.listInstances(undefined, workflowId!),
    enabled: !!workflowId,
    staleTime: 10_000,
  });
}

export function useWorkflowInstance(id: string | undefined) {
  return useQuery({ queryKey: ["workflow-instance", id], queryFn: () => workflowsApi.getInstance(id!), enabled: !!id, staleTime: 5_000 });
}

export function useRunWorkflow() {
  const qc = useQueryClient();
  return useMutation({ mutationFn: ({ id, payload, owner }: { id: string; payload?: unknown; owner?: string | null }) => workflowsApi.run(id, payload, undefined, undefined, owner), onSuccess: () => { qc.invalidateQueries({ queryKey: ["workflow-instances"] }); } });
}

export function useResumeWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, outcome, gate }: { id: string; outcome?: string; gate?: string }) =>
      workflowsApi.resume(id, outcome, gate),
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["workflow-instances"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance-steps"] });
    },
  });
}

export function useAbortWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => workflowsApi.abort(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workflow-instances"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance"] });
    },
  });
}

export function useInstanceSteps(id: string | undefined) {
  return useQuery({
    queryKey: ["workflow-instance-steps", id],
    queryFn: () => workflowsApi.getSteps(id!),
    enabled: !!id,
    staleTime: 5_000,
  });
}

export function useInstanceCost(id: string | undefined) {
  return useQuery({
    queryKey: ["workflow-instance-cost", id],
    queryFn: () => workflowsApi.getCost(id!),
    enabled: !!id,
    staleTime: 5_000,
  });
}

export function useInstanceVariables(id: string | undefined) {
  return useQuery({
    queryKey: ["workflow-instance-variables", id],
    queryFn: () => workflowsApi.getVariables(id!),
    enabled: !!id,
    staleTime: 5_000,
  });
}

export function useRejectWorkflow() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, reason, feedback }: { id: string; reason?: string; feedback?: string }) =>
      workflowsApi.reject(id, reason, feedback),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workflow-instances"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance"] });
    },
  });
}

export function useRetryStep() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ instanceId, stepId }: { instanceId: string; stepId: string }) =>
      workflowsApi.retryStep(instanceId, stepId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workflow-instances"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance-steps"] });
    },
  });
}

export function useSkipStep() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ instanceId, stepId }: { instanceId: string; stepId: string }) =>
      workflowsApi.skipStep(instanceId, stepId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workflow-instances"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance-steps"] });
    },
  });
}

export function useGotoStep() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ instanceId, stepId }: { instanceId: string; stepId: string }) =>
      workflowsApi.gotoStep(instanceId, stepId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["workflow-instances"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance"] });
      qc.invalidateQueries({ queryKey: ["workflow-instance-steps"] });
    },
  });
}

export function useExportWorkflows() {
  return useQuery({
    queryKey: ["workflows-export"],
    queryFn: workflowsApi.export,
    enabled: false, // only fetch on demand
  });
}
