import { gateway } from "../api-client";

export type ProcessingStatus = "pending" | "processing" | "ready" | "failed";

export type SourceKind = "cli" | "http_upload" | "slack" | "web_ui";

export interface CorpusSummary {
  id: string;
  name: string;
  document_count: number;
  chunk_count: number;
  created_at: string;
  created_by: string;
}

export interface CorpusListResponse {
  corpora: CorpusSummary[];
}

export interface DocumentRow {
  id: string;
  short_id: string;
  filename: string;
  mime_type: string;
  size_bytes: number;
  source_kind: SourceKind;
  processing_status: ProcessingStatus;
  processing_error?: string | null;
  uploaded_at: string;
  uploaded_by: string;
  chunk_count: number;
}

export interface CorpusDetail {
  id: string;
  name: string;
  created_at: string;
  created_by: string;
  documents: DocumentRow[];
}

export interface UploadOutcome {
  agent_id: string;
  corpus: string;
  filename: string;
  corpus_id: string | null;
  document_id: string | null;
  document_short: string | null;
  written: number;
  unchanged: number;
  deleted: number;
}

export interface DeleteCorpusOutcome {
  agent_id: string;
  corpus: string;
  chunks_removed: number;
  memories_forgotten: number;
  files_unlinked: number;
}

export interface DeleteDocumentOutcome {
  agent_id: string;
  document_id: string;
  corpus: string;
  chunks_removed: number;
  memories_forgotten: number;
  files_unlinked: number;
}

function corporaPath(agentId: string): string {
  return `/api/agents/${encodeURIComponent(agentId)}/training/corpora`;
}

function corpusPath(agentId: string, corpusName: string): string {
  return `${corporaPath(agentId)}/${encodeURIComponent(corpusName)}`;
}

function documentPath(agentId: string, docId: string): string {
  return `/api/agents/${encodeURIComponent(agentId)}/training/documents/${encodeURIComponent(docId)}`;
}

export interface DocumentMeta {
  id: string;
  short_id: string;
  corpus: string;
  filename: string;
  mime_type: string;
  size_bytes: number;
  source_kind: SourceKind;
  processing_status: ProcessingStatus;
  processing_error?: string | null;
  uploaded_at: string;
  uploaded_by: string;
  chunk_count: number;
}

export const trainingApi = {
  listCorpora: (agentId: string) =>
    gateway.get<CorpusListResponse>(corporaPath(agentId)),

  getCorpus: (agentId: string, corpusName: string) =>
    gateway.get<CorpusDetail>(corpusPath(agentId, corpusName)),

  /** Look up a single document by full UUID or 8-char short id. */
  getDocument: (agentId: string, docRef: string) =>
    gateway.get<DocumentMeta>(documentPath(agentId, docRef)),

  uploadCorpus: (agentId: string, corpus: string, file: File) => {
    const form = new FormData();
    form.append("corpus", corpus);
    form.append("file", file, file.name);
    return gateway.postFormData<UploadOutcome>(corporaPath(agentId), form);
  },

  deleteCorpus: (agentId: string, corpusName: string) =>
    gateway.del<DeleteCorpusOutcome>(corpusPath(agentId, corpusName)),

  deleteDocument: (agentId: string, docId: string) =>
    gateway.del<DeleteDocumentOutcome>(documentPath(agentId, docId)),

  /** URL for the hub's chunked-transfer download. Open in `window.location`
   * or an `<a download>` to let the browser handle the save dialog. */
  documentDownloadUrl: (agentId: string, docId: string): string =>
    `${documentPath(agentId, docId)}/download`,
};
