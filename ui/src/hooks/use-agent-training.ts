import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { trainingApi } from "@/lib/api/training";
import type {
  CorpusDetail,
  CorpusListResponse,
  DocumentMeta,
  UploadOutcome,
  DeleteCorpusOutcome,
  DeleteDocumentOutcome,
} from "@/lib/api/training";

export type {
  CorpusSummary,
  CorpusDetail,
  DocumentMeta,
  DocumentRow,
  ProcessingStatus,
  SourceKind,
  UploadOutcome,
  DeleteCorpusOutcome,
  DeleteDocumentOutcome,
} from "@/lib/api/training";

export function useTrainingDocument(
  agentId: string | undefined,
  docRef: string | null,
) {
  return useQuery<DocumentMeta>({
    queryKey: ["training-document", agentId, docRef],
    queryFn: () => trainingApi.getDocument(agentId!, docRef!),
    enabled: !!agentId && !!docRef,
    staleTime: 5 * 60_000,
  });
}

export function useAgentCorpora(agentId: string | undefined) {
  return useQuery<CorpusListResponse>({
    queryKey: ["agent-training-corpora", agentId],
    queryFn: () => trainingApi.listCorpora(agentId!),
    enabled: !!agentId,
    staleTime: 15_000,
  });
}

export function useAgentCorpusDetail(
  agentId: string | undefined,
  corpusName: string | null,
) {
  return useQuery<CorpusDetail>({
    queryKey: ["agent-training-corpus", agentId, corpusName],
    queryFn: () => trainingApi.getCorpus(agentId!, corpusName!),
    enabled: !!agentId && !!corpusName,
    staleTime: 10_000,
  });
}

export function useUploadAgentCorpus(agentId: string) {
  const qc = useQueryClient();
  return useMutation<UploadOutcome, Error, { corpus: string; file: File }>({
    mutationFn: ({ corpus, file }) =>
      trainingApi.uploadCorpus(agentId, corpus, file),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ["agent-training-corpora", agentId] });
      qc.invalidateQueries({
        queryKey: ["agent-training-corpus", agentId, vars.corpus],
      });
    },
  });
}

export function useDeleteAgentCorpus(agentId: string) {
  const qc = useQueryClient();
  return useMutation<DeleteCorpusOutcome, Error, string>({
    mutationFn: (corpusName) => trainingApi.deleteCorpus(agentId, corpusName),
    onSuccess: (_data, corpusName) => {
      qc.invalidateQueries({ queryKey: ["agent-training-corpora", agentId] });
      qc.removeQueries({
        queryKey: ["agent-training-corpus", agentId, corpusName],
      });
      // Same reason as in `useDeleteAgentDocument`: stale citation
      // chips must invalidate when a corpus is wiped.
      qc.invalidateQueries({ queryKey: ["training-document", agentId] });
    },
  });
}

export function useDeleteAgentDocument(
  agentId: string,
  corpusName: string | null,
) {
  const qc = useQueryClient();
  return useMutation<DeleteDocumentOutcome, Error, string>({
    mutationFn: (docId) => trainingApi.deleteDocument(agentId, docId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["agent-training-corpora", agentId] });
      if (corpusName) {
        qc.invalidateQueries({
          queryKey: ["agent-training-corpus", agentId, corpusName],
        });
      }
      // Citation chips cache per-doc metadata aggressively (5 min). A
      // delete invalidates all of them so stale chips can't survive
      // pointing at gone documents.
      qc.invalidateQueries({ queryKey: ["training-document", agentId] });
    },
  });
}
