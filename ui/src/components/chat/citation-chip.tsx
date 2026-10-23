import { FileText } from "lucide-react";
import { useTrainingDocument } from "@/hooks/use-agent-training";
import { trainingApi } from "@/lib/api/training";

interface CitationChipProps {
  agentId: string;
  shortId: string;
}

const BASE_CLASSES =
  "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-xs font-medium align-baseline";

export function CitationChip({ agentId, shortId }: CitationChipProps) {
  const { data, isLoading, isError } = useTrainingDocument(agentId, shortId);

  if (isLoading) {
    return (
      <span className={`${BASE_CLASSES} bg-muted text-muted-foreground`}>
        <FileText className="h-3 w-3" />
        <span>…</span>
      </span>
    );
  }

  if (isError || !data) {
    return (
      <span
        className={`${BASE_CLASSES} bg-muted text-muted-foreground`}
        title={`Document doc:${shortId} not found for this agent`}
      >
        <FileText className="h-3 w-3" />
        <span>doc:{shortId}</span>
      </span>
    );
  }

  const tooltip = data.corpus
    ? `${data.filename} (${data.corpus})`
    : data.filename;

  return (
    <a
      href={trainingApi.documentDownloadUrl(agentId, data.id)}
      download={data.filename}
      target="_blank"
      rel="noopener noreferrer"
      className={`${BASE_CLASSES} bg-blue-500/10 hover:bg-blue-500/20 text-blue-400 no-underline hover:no-underline`}
      title={tooltip}
    >
      <FileText className="h-3 w-3" />
      <span>{data.filename}</span>
    </a>
  );
}
