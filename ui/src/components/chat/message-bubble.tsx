import { memo, useMemo, useState, useCallback, type ReactNode, type HTMLAttributes } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import {
  Copy,
  Check,
  Loader2,
  AlertCircle,
  ChevronRight,
  User,
  Bot,
  Wrench,
  CheckCircle2,
  Download,
  Paperclip,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { gateway } from "@/lib/api-client";
import { downloadBlob } from "@/lib/download";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import type { CodeCitation, MessageStatus, ToolCall, WireAttachment } from "@/stores/chat-store";
import { CitationChip } from "@/components/chat/citation-chip";
import { CodeCitationChip } from "@/components/chat/code-citation-chip";
import { ConnectPromptCard } from "@/components/chat/connect-prompt-card";
import type { ConnectPrompt } from "@/lib/api/subscriptions";

const LONG_MESSAGE_THRESHOLD = 4000;

// Training citation marker — produced by the LLM when it cites a
// training document. We rewrite each occurrence into a markdown
// link with a custom scheme so the `a` override below can render it
// as a CitationChip without breaking react-markdown's pipeline.
const CITE_RE = /\[\[cite:([a-f0-9]{1,16})\]\]/g;
const CITE_SCHEME = "workforce-cite://";

function preprocessCitations(text: string): string {
  return text.replace(CITE_RE, (_, shortId) => `[${shortId}](${CITE_SCHEME}${shortId})`);
}

// ── Types ────────────────────────────────────────────────────────────────────

interface MessageBubbleProps {
  role: string;
  content: string;
  timestamp?: string;
  status?: MessageStatus;
  toolCalls?: ToolCall[];
  toolMetadata?: { name: string; success: boolean };
  className?: string;
  /** Agent owning this message — required to resolve `[[cite:...]]` markers. */
  agentId?: string;
  /** Deterministic source citations for the grounded files/line-ranges. */
  codeCitations?: CodeCitation[];
  /** Which route and whose plan served the answer. */
  routeLine?: string;
  /** A login the turn needed, offered to the person to connect. */
  connectPrompt?: ConnectPrompt;
  /** Failure reason for status === "error" — rendered alongside the content
   *  (a failed send shows the user's text plus WHY it didn't go out). */
  error?: string;
  /** Files a tool delivered: images render inline, anything else downloads
   *  from the session's file route. */
  attachments?: WireAttachment[];
  /** The session the attachments belong to — what the file route is keyed by. */
  sessionId?: string;
}

// Above this many citations, the Sources row collapses behind a toggle so a
// large grounded answer doesn't push a wall of chips under it.
const CITATIONS_COLLAPSE_THRESHOLD = 6;

// ── Code Citations (Sources) ─────────────────────────────────────────────────

function CitationChips({ citations }: { citations: CodeCitation[] }) {
  return (
    <div className="flex flex-wrap gap-1.5">
      {citations.map((c, i) => (
        <CodeCitationChip key={`${c.path}:${c.start_line}-${i}`} citation={c} />
      ))}
    </div>
  );
}

function CodeCitations({ citations }: { citations: CodeCitation[] }) {
  const collapsed = citations.length > CITATIONS_COLLAPSE_THRESHOLD;
  return (
    <div className="mt-2 border-t border-border/40 pt-2">
      {collapsed ? (
        <CollapsibleSection
          defaultOpen={false}
          triggerLabel={(open) =>
            open ? "Hide sources" : `Sources (${citations.length})`
          }
        >
          <CitationChips citations={citations} />
        </CollapsibleSection>
      ) : (
        <>
          <div className="mb-1.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Sources
          </div>
          <CitationChips citations={citations} />
        </>
      )}
    </div>
  );
}

// ── Delivered files ──────────────────────────────────────────────────────────

function AttachmentList({ attachments, sessionId }: { attachments: WireAttachment[]; sessionId?: string }) {
  // Built once per message: a data URL of a few MB must not be rebuilt on
  // every stream delta the page renders after it.
  const images = useMemo(
    () =>
      attachments.map((file) =>
        file.mime_type.startsWith("image/") && file.data
          ? `data:${file.mime_type};base64,${file.data}`
          : null,
      ),
    [attachments],
  );
  return (
    <div className="flex flex-col gap-2 mb-2">
      {attachments.map((file, i) =>
        images[i] ? (
          <figure key={`${file.filename}-${i}`} className="m-0">
            <img
              src={images[i]}
              alt={file.filename}
              className="max-w-full rounded-lg border border-border/50 bg-white"
            />
            <figcaption className="mt-1 text-[10px] text-muted-foreground">{file.filename}</figcaption>
          </figure>
        ) : (
          <FileChip key={`${file.filename}-${i}`} file={file} sessionId={sessionId} />
        ),
      )}
    </div>
  );
}

function FileChip({ file, sessionId }: { file: WireAttachment; sessionId?: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const path = file.artifact_id && sessionId
    ? `/api/sessions/${encodeURIComponent(sessionId)}/files/${encodeURIComponent(file.artifact_id)}`
    : null;
  const handleDownload = useCallback(async () => {
    if (!path) return;
    setBusy(true);
    setError(null);
    try {
      downloadBlob(await gateway.getBlob(path), file.filename);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [path, file.filename]);

  return (
    <button
      type="button"
      onClick={handleDownload}
      disabled={!path || busy}
      title={path ? error ?? undefined : "This file has no download reference"}
      className={cn(
        "inline-flex w-fit items-center gap-2 rounded-md border border-border bg-muted/50 px-3 py-1.5 text-xs hover:bg-muted disabled:opacity-60",
        error && "border-red-500/40 text-red-400",
      )}
    >
      <Paperclip className="h-3.5 w-3.5" />
      <span className="font-mono">{file.filename}</span>
      {error && <span>{error}</span>}
      {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Download className="h-3.5 w-3.5" />}
    </button>
  );
}

// ── Copy Button ──────────────────────────────────────────────────────────────

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [text]);

  return (
    <button
      onClick={handleCopy}
      className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
      aria-label="Copy code"
    >
      {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

// ── Code Block ───────────────────────────────────────────────────────────────

function CodeBlock({
  children,
  className,
  ...props
}: HTMLAttributes<HTMLElement> & { children?: ReactNode }) {
  // Inline code (no language class)
  const isBlock = typeof className === "string" && className.startsWith("hljs");
  if (!isBlock) {
    return (
      <code className="rounded bg-muted px-1.5 py-0.5 text-xs font-mono" {...props}>
        {children}
      </code>
    );
  }

  // Extract language from hljs class
  const lang = className
    ?.split(" ")
    .find((c) => c.startsWith("language-"))
    ?.replace("language-", "") ?? "";

  // Collect raw text for copy
  const raw = extractText(children);

  return (
    <div className="group/code relative my-3 rounded-lg border bg-[#0d1117] overflow-hidden">
      <div className="flex items-center justify-between px-4 py-2 bg-muted/30 border-b">
        <span className="text-xs text-muted-foreground font-mono">{lang || "code"}</span>
        <CopyButton text={raw} />
      </div>
      <pre className="overflow-x-auto p-4 text-sm !bg-transparent !m-0">
        <code className={className} {...props}>
          {children}
        </code>
      </pre>
    </div>
  );
}

function extractText(node: ReactNode): string {
  if (typeof node === "string") return node;
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (node && typeof node === "object" && "props" in node) {
    return extractText((node as { props: { children?: ReactNode } }).props.children);
  }
  return "";
}

// ── Tool Call Card ───────────────────────────────────────────────────────────

function ToolCallCard({ tool }: { tool: ToolCall }) {
  return (
    <div
      className={cn(
        "flex items-center gap-2 text-xs rounded-md border px-3 py-1.5",
        tool.active
          ? "border-blue-500/30 bg-blue-500/10 text-blue-400"
          : "border-border bg-muted/50 text-muted-foreground",
      )}
    >
      {tool.active ? (
        <Loader2 className="h-3 w-3 animate-spin" />
      ) : (
        <CheckCircle2 className="h-3 w-3 text-green-500" />
      )}
      <Wrench className="h-3 w-3" />
      <span className="font-mono">{tool.name}</span>
    </div>
  );
}

// ── Tool Metadata (for transcript entries) ───────────────────────────────────

function ToolMetadataCard({ meta }: { meta: { name: string; success: boolean } }) {
  return (
    <div
      className={cn(
        "flex items-center gap-2 text-xs rounded-md border px-3 py-1.5",
        meta.success
          ? "border-green-500/30 bg-green-500/10 text-green-400"
          : "border-red-500/30 bg-red-500/10 text-red-400",
      )}
    >
      {meta.success ? (
        <CheckCircle2 className="h-3 w-3" />
      ) : (
        <AlertCircle className="h-3 w-3" />
      )}
      <Wrench className="h-3 w-3" />
      <span className="font-mono">{meta.name}</span>
    </div>
  );
}

// ── Streaming Cursor ─────────────────────────────────────────────────────────

function StreamingCursor() {
  return <span className="inline-block w-2 h-4 bg-foreground/70 animate-pulse ml-0.5 align-text-bottom" />;
}

// ── Role Icon ────────────────────────────────────────────────────────────────

function RoleIcon({ role }: { role: string }) {
  const r = role.toLowerCase();
  if (r === "user") {
    return (
      <div className="flex items-center justify-center w-7 h-7 rounded-full bg-primary text-primary-foreground shrink-0">
        <User className="h-3.5 w-3.5" />
      </div>
    );
  }
  if (r === "assistant") {
    return (
      <div className="flex items-center justify-center w-7 h-7 rounded-full bg-violet-600 text-white shrink-0">
        <Bot className="h-3.5 w-3.5" />
      </div>
    );
  }
  if (r === "tool") {
    return (
      <div className="flex items-center justify-center w-7 h-7 rounded-full bg-amber-600/20 text-amber-500 shrink-0">
        <Wrench className="h-3.5 w-3.5" />
      </div>
    );
  }
  return (
    <div className="flex items-center justify-center w-7 h-7 rounded-full bg-muted text-muted-foreground shrink-0">
      <AlertCircle className="h-3.5 w-3.5" />
    </div>
  );
}

// ── Timestamp ────────────────────────────────────────────────────────────────

function formatRelative(ts: string): string {
  const diff = Date.now() - new Date(ts).getTime();
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(ts).toLocaleDateString();
}

// ── Markdown Components Override ─────────────────────────────────────────────

function buildMarkdownComponents(agentId?: string) {
  return {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    code: (props: any) => <CodeBlock {...props} />,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    pre: ({ children }: any) => <>{children}</>,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    a: ({ children, href, ...props }: any) => {
      if (
        typeof href === "string" &&
        href.startsWith(CITE_SCHEME) &&
        agentId
      ) {
        const shortId = href.slice(CITE_SCHEME.length);
        return <CitationChip agentId={agentId} shortId={shortId} />;
      }
      return (
        <a
          href={href}
          target="_blank"
          rel="noopener noreferrer"
          className="text-blue-400 hover:underline"
          {...props}
        >
          {children}
        </a>
      );
    },
  };
}

// ── Main Component ───────────────────────────────────────────────────────────

function CollapseChevron({ open }: { open: boolean }) {
  return (
    <ChevronRight
      className={cn(
        "h-3.5 w-3.5 transition-transform",
        open && "rotate-90",
      )}
    />
  );
}

function CollapsibleSection({
  triggerLabel,
  defaultOpen,
  children,
}: {
  triggerLabel: (open: boolean) => string;
  defaultOpen: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger className="mb-1.5">
        <CollapseChevron open={open} />
        {triggerLabel(open)}
      </CollapsibleTrigger>
      <CollapsibleContent>{children}</CollapsibleContent>
    </Collapsible>
  );
}

export const MessageBubble = memo(function MessageBubble({
  role,
  content,
  timestamp,
  status = "complete",
  toolCalls = [],
  toolMetadata,
  className,
  agentId,
  codeCitations,
  routeLine,
  connectPrompt,
  error,
  attachments,
  sessionId,
}: MessageBubbleProps) {
  const transformedContent = useMemo(
    () => (agentId ? preprocessCitations(content) : content),
    [content, agentId],
  );
  const markdownComponents = useMemo(
    () => buildMarkdownComponents(agentId),
    [agentId],
  );
  const r = role.toLowerCase();
  const isUser = r === "user";
  const isSystem = r === "system";
  const isError = status === "error";
  const isStreaming = status === "streaming";

  // Error messages (status === "error") — distinct red error block. A failed
  // user send keeps the typed text visible and states the reason separately;
  // server-emitted error messages carry the reason as their content.
  if (isError) {
    return (
      <div className={cn("flex gap-3 py-3", className)}>
        <RoleIcon role={error && isUser ? "user" : "system"} />
        <div className="flex-1 min-w-0">
          {error && content && (
            <div className="text-sm whitespace-pre-wrap mb-2">{content}</div>
          )}
          <div className="flex items-center gap-2 rounded-lg border border-red-500/20 bg-red-500/5 px-4 py-3">
            <AlertCircle className="h-4 w-4 text-red-500 shrink-0" />
            <p className="text-sm text-red-400">{error ?? content}</p>
          </div>
          {timestamp && (
            <span className="text-[10px] text-muted-foreground mt-1 block">{formatRelative(timestamp)}</span>
          )}
        </div>
      </div>
    );
  }

  if (connectPrompt) {
    return (
      <div className={cn("flex gap-3 py-3", className)}>
        <RoleIcon role="system" />
        <div className="flex-1 min-w-0">
          <ConnectPromptCard prompt={connectPrompt} />
        </div>
      </div>
    );
  }

  // System messages — collapsed by default. The full system prompt for an
  // agent is several thousand chars; expanded inline it dominates the page.
  if (isSystem) {
    return (
      <div className={cn("flex gap-3 py-3", className)}>
        <RoleIcon role="system" />
        <div className="flex-1 min-w-0">
          <CollapsibleSection
            defaultOpen={false}
            triggerLabel={(open) =>
              open
                ? `Hide system context (${content.length.toLocaleString()} chars)`
                : `Show system context (${content.length.toLocaleString()} chars)`
            }
          >
            <div className="rounded-md border border-border/50 bg-muted/30 px-3 py-2 text-xs text-muted-foreground whitespace-pre-wrap font-mono">
              {content}
            </div>
          </CollapsibleSection>
          {timestamp && (
            <span className="text-[10px] text-muted-foreground mt-1 block">{formatRelative(timestamp)}</span>
          )}
        </div>
      </div>
    );
  }

  // Tool role (transcript entries with metadata)
  if (r === "tool") {
    return (
      <div className={cn("flex gap-3 py-2", className)}>
        <RoleIcon role="tool" />
        <div className="flex-1 min-w-0">
          {toolMetadata && <ToolMetadataCard meta={toolMetadata} />}
          {content && (
            <div className="mt-1 text-xs text-muted-foreground font-mono bg-muted/50 rounded-md px-3 py-2 max-h-40 overflow-y-auto whitespace-pre-wrap">
              {content}
            </div>
          )}
        </div>
      </div>
    );
  }

  const isLong = content.length > LONG_MESSAGE_THRESHOLD && !isStreaming;

  // User message
  if (isUser) {
    const body = <div className="text-sm whitespace-pre-wrap">{content}</div>;
    return (
      <div className={cn("flex gap-3 py-3", className)}>
        <RoleIcon role="user" />
        <div className="flex-1 min-w-0">
          {isLong ? (
            <CollapsibleSection
              defaultOpen
              triggerLabel={(open) =>
                open
                  ? `Collapse (${content.length.toLocaleString()} chars)`
                  : `Expand (${content.length.toLocaleString()} chars)`
              }
            >
              {body}
            </CollapsibleSection>
          ) : (
            body
          )}
          {status === "sending" ? (
            <span className="flex items-center gap-1.5 text-[10px] text-muted-foreground mt-1">
              <Loader2 className="h-3 w-3 animate-spin" />
              Uploading attachment… keep this page open
            </span>
          ) : (
            timestamp && (
              <span className="text-[10px] text-muted-foreground mt-1 block">{formatRelative(timestamp)}</span>
            )
          )}
        </div>
      </div>
    );
  }

  // Assistant message
  const markdownBody = content && (
    <div className="prose prose-sm dark:prose-invert max-w-none prose-pre:bg-transparent prose-pre:p-0 prose-pre:m-0 prose-code:before:content-none prose-code:after:content-none prose-table:border prose-th:border prose-th:px-3 prose-th:py-1.5 prose-td:border prose-td:px-3 prose-td:py-1.5 prose-thead:bg-muted/50 break-words">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
        components={markdownComponents}
      >
        {transformedContent}
      </ReactMarkdown>
      {isStreaming && <StreamingCursor />}
    </div>
  );

  return (
    <div className={cn("flex gap-3 py-3", className)}>
      <RoleIcon role="assistant" />
      <div className="flex-1 min-w-0">
        {/* Tool calls */}
        {toolCalls.length > 0 && (
          <div className="flex flex-wrap gap-1.5 mb-2">
            {toolCalls.map((tc, i) => (
              <ToolCallCard key={`${tc.name}-${i}`} tool={tc} />
            ))}
          </div>
        )}

        {attachments && attachments.length > 0 && (
          <AttachmentList attachments={attachments} sessionId={sessionId} />
        )}

        {isLong && markdownBody ? (
          <CollapsibleSection
            defaultOpen
            triggerLabel={(open) =>
              open
                ? `Collapse (${content.length.toLocaleString()} chars)`
                : `Expand (${content.length.toLocaleString()} chars)`
            }
          >
            {markdownBody}
          </CollapsibleSection>
        ) : (
          markdownBody
        )}

        {/* Streaming with no content yet */}
        {!content && isStreaming && (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          </div>
        )}

        {/* Deterministic source citations under the answer */}
        {codeCitations && codeCitations.length > 0 && (
          <CodeCitations citations={codeCitations} />
        )}

        {routeLine && <p className="mt-1.5 text-[11px] text-muted-foreground">{routeLine}</p>}

        {timestamp && status === "complete" && (
          <span className="text-[10px] text-muted-foreground mt-1.5 block">{formatRelative(timestamp)}</span>
        )}
      </div>
    </div>
  );
});
