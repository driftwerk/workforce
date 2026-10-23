import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  agentTransferApi,
  type ImportOptions,
} from "@/lib/api/agent-transfer";

const onError = (action: string) => (err: unknown) =>
  window.alert(
    `${action} failed: ${err instanceof Error ? err.message : String(err)}`,
  );

export function useAgentTransfer() {
  const qc = useQueryClient();
  return {
    exportBundle: useMutation({
      mutationFn: agentTransferApi.exportBundle,
      onError: onError("Export"),
    }),
    uploadBundle: useMutation({
      mutationFn: agentTransferApi.uploadBundle,
      onError: onError("Upload bundle"),
    }),
    applyImport: useMutation({
      mutationFn: ({
        artifactId,
        options,
      }: {
        artifactId: string;
        options: ImportOptions;
      }) => agentTransferApi.applyImport(artifactId, options),
      onSuccess: () => qc.invalidateQueries({ queryKey: ["agents"] }),
      onError: onError("Apply import"),
    }),
  };
}
