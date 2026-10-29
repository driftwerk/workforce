import { create } from "zustand";
import { useAuthStore } from "./auth-store";
import { gateway, ApiError } from "@/lib/api-client";
import { sessionsApi } from "@/lib/api/sessions";
import type { ConnectPrompt } from "@/lib/api/subscriptions";
import {
  assistantMessage,
  attachToLastAnswer,
  attachmentMessage,
  closeStopped,
  closeStreaming,
  discardCall,
  insertBeforeStreaming,
  isStoppedTail,
  toolEnded,
  toolStarted,
  updateToolCalls,
} from "./attach";
import {
  answerIn,
  outstandingQuestion,
  repeatsCollected,
  withAnswer,
  type PendingAnswer,
} from "@/lib/chat-resume";

export type MessageStatus = "sending" | "streaming" | "complete" | "error";

export interface ToolCall {
  name: string;
  active: boolean;
}

/**
 * A deterministic code citation — the exact file/line-range the answer-ready
 * `code_context` injector grounded an answer in. Wire shape mirrors the Rust
 * `workforce_core::code_citation::CodeCitation`. `owner`/`repo_name` are empty
 * when the source isn't a GitHub target (the chip then renders as plain text).
 */
export interface CodeCitation {
  repo: string;
  owner: string;
  repo_name: string;
  commit: string;
  path: string;
  start_line: number;
  end_line: number;
  kind: string;
  symbol: string;
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  timestamp: string;
  status: MessageStatus;
  toolCalls: ToolCall[];
  error?: string;
  /** Source chips for the grounded files/line-ranges (set after the answer). */
  codeCitations?: CodeCitation[];
  /** Which route and whose plan served the answer (set after the answer). */
  routeLine?: string;
  /** A login the turn needed, for the person to connect. */
  connectPrompt?: ConnectPrompt;
  /** Files a tool delivered during the turn: images inline, others by
   *  artifact reference. */
  attachments?: WireAttachment[];
  /** The session a delivered file is served from. */
  sessionId?: string;
}

interface ServerMessage {
  type: string;
  content?: string;
  session_id?: string;
  error?: string;
  agent_id?: string;
  context_tokens?: number;
  context_max?: number;
  code_citations?: CodeCitation[];
  connect_prompt?: ConnectPrompt;
  route?: string;
  attachments?: WireAttachment[];
}

interface ChatState {
  // Connection
  connected: boolean;
  connecting: boolean;
  agentId: string | null;

  // Messages
  messages: ChatMessage[];
  sessionId: string | null;

  // Streaming state
  isStreaming: boolean;
  isTyping: boolean;
  isCompacting: boolean;
  /** A large attachment is uploading ahead of its message — the input
   *  disables so a second send can't overtake it out of order. */
  isUploading: boolean;

  // Context window
  contextTokens: number | null;
  contextMax: number | null;

  // Actions
  connect: (agentId: string) => void;
  disconnect: () => void;
  send: (content: string, attachments?: File[]) => void;
  cancel: () => void;
  clearMessages: () => void;
}

let ws: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempts = 0;
let pingInterval: ReturnType<typeof setInterval> | null = null;
let msgCounter = 0;
/** The answer still coming when the socket went. A hub restarted mid-turn
 *  leaves the turn to another, which saves the answer to the session and
 *  pushes it to the page if the page is connected by then. */
let pending: PendingAnswer | null = null;
/** The answer last collected from the session, which its push repeats. */
let collected: { sessionId: string; content: string } | null = null;
// The session whose turn the user stopped, until they send again: what that
// turn still had on the wire must not reopen what the stop closed.
let stopped: string | null = null;
/** The answers the current model call has streamed: what a `stream_discard`
 *  takes back. Each call opens with a `typing`. */
let callStream: string[] = [];

function nextId(): string {
  return `msg-${Date.now()}-${++msgCounter}`;
}

function wsUrl(agentId: string): string {
  const token = useAuthStore.getState().token ?? "";
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  const host = window.location.host;
  return `${proto}//${host}/ws?token=${encodeURIComponent(token)}&agent_id=${encodeURIComponent(agentId)}`;
}

function cleanupConnection() {
  if (pingInterval) {
    clearInterval(pingInterval);
    pingInterval = null;
  }
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  if (ws) {
    ws.onopen = null;
    ws.onclose = null;
    ws.onerror = null;
    ws.onmessage = null;
    if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) {
      ws.close();
    }
    ws = null;
  }
}

export const useChatStore = create<ChatState>((set, get) => ({
  connected: false,
  connecting: false,
  agentId: null,
  messages: [],
  sessionId: null,
  isStreaming: false,
  isTyping: false,
  isCompacting: false,
  isUploading: false,
  contextTokens: null,
  contextMax: null,

  connect: (agentId: string) => {
    cleanupConnection();
    reconnectAttempts = 0;
    set({ agentId, connecting: true, connected: false });
    openSocket(agentId);
  },

  disconnect: () => {
    awaitAnswer();
    cleanupConnection();
    reconnectAttempts = 0;
    set({ connected: false, connecting: false, agentId: null });
  },

  send: async (content: string, attachments?: File[]) => {
    // Captured up front: uploads take minutes, and `ws`/`agentId` are
    // reassigned by reconnects and agent switches. The message must go out
    // on the socket (and to the agent) the user sent it under, or not at all.
    const socket = ws;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    const agentIdAtSend = get().agentId;

    const large = (attachments ?? []).filter(
      (f) => f.size > INLINE_ATTACHMENT_MAX_BYTES,
    );
    const userMsg: ChatMessage = {
      id: nextId(),
      role: "user",
      content,
      timestamp: new Date().toISOString(),
      // Large files upload before the message goes out; show that.
      status: large.length > 0 ? "sending" : "complete",
      toolCalls: [],
    };
    pending = null;
    collected = null;
    stopped = null;
    set((s) => ({ messages: [...s.messages, userMsg] }));
    const failSend = (error: string) =>
      set((s) => ({
        messages: s.messages.map((m) =>
          m.id === userMsg.id ? { ...m, status: "error" as const, error } : m,
        ),
      }));

    const oversized = large.find((f) => f.size > MAX_UPLOAD_FILE_BYTES);
    if (oversized) {
      const gib = (oversized.size / (1024 * 1024 * 1024)).toFixed(1);
      failSend(
        `"${oversized.name}" is ${gib} GiB — over the 1 GiB upload limit. ` +
          "Trim or compress it and try again.",
      );
      return;
    }

    // Build attachments payload: small files ride inline; big ones upload to
    // the session first and ride by artifact reference — the WS message (and
    // everything downstream of it) never carries their bytes.
    let attachmentPayload: WireAttachment[] | undefined;
    if (attachments && attachments.length > 0) {
      try {
        let uploadSessionId: string | null = null;
        if (large.length > 0) {
          set({ isUploading: true });
          uploadSessionId = await awaitSessionId();
          if (!uploadSessionId) {
            throw new Error(
              "No session established yet — send a short text message first, then retry the upload.",
            );
          }
        }
        attachmentPayload = await Promise.all(
          attachments.map(async (f) => {
            if (f.size <= INLINE_ATTACHMENT_MAX_BYTES) {
              return {
                filename: f.name,
                mime_type: f.type || "application/octet-stream",
                data: await fileToWireData(f),
              };
            }
            return uploadLargeFile(f, uploadSessionId as string, agentIdAtSend);
          }),
        );
      } catch (e) {
        failSend(e instanceof Error ? e.message : `Attachment upload failed: ${e}`);
        return;
      } finally {
        if (large.length > 0) set({ isUploading: false });
      }
    }

    if (ws !== socket || socket.readyState !== WebSocket.OPEN) {
      failSend("Connection changed while uploading — please resend.");
      return;
    }
    if (get().agentId !== agentIdAtSend) {
      failSend("Agent changed while uploading — please resend.");
      return;
    }
    const msg: Record<string, unknown> = {
      type: "chat",
      content,
      session_id: get().sessionId,
      channel: "web",
      agent_id: agentIdAtSend,
    };
    if (attachmentPayload) {
      msg.attachments = attachmentPayload;
    }

    socket.send(JSON.stringify(msg));
    if (large.length > 0) {
      set((s) => ({
        messages: s.messages.map((m) =>
          m.id === userMsg.id ? { ...m, status: "complete" as const } : m,
        ),
      }));
    }
  },

  cancel: () => {
    const { sessionId } = get();
    if (!ws || ws.readyState !== WebSocket.OPEN || !sessionId) return;
    ws.send(JSON.stringify({ type: "cancel", session_id: sessionId }));
    pending = null;
    stopped = sessionId;
    callStream = [];
    set((s) => ({
      messages: closeStopped(s.messages),
      isStreaming: false,
      isTyping: false,
      isCompacting: false,
    }));
  },

  clearMessages: () => {
    pending = null;
    collected = null;
    stopped = null;
    callStream = [];
    set({ messages: [], sessionId: null, isStreaming: false, isTyping: false, isCompacting: false });
  },
}));

function openSocket(agentId: string) {
  const url = wsUrl(agentId);
  ws = new WebSocket(url);

  ws.onopen = () => {
    reconnectAttempts = 0;
    useChatStore.setState({ connected: true, connecting: false });

    // Keepalive ping every 30s
    pingInterval = setInterval(() => {
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "ping" }));
      }
    }, 30_000);
  };

  ws.onclose = () => {
    awaitAnswer();
    useChatStore.setState({ connected: false, connecting: false });
    if (pingInterval) {
      clearInterval(pingInterval);
      pingInterval = null;
    }
    scheduleReconnect(agentId);
  };

  ws.onerror = () => {};

  ws.onmessage = (event) => {
    try {
      const msg: ServerMessage = JSON.parse(event.data);
      handleServerMessage(msg);
    } catch {
      // ignore malformed messages
    }
  };
}

function scheduleReconnect(agentId: string) {
  const currentAgentId = useChatStore.getState().agentId;
  if (currentAgentId !== agentId) return; // disconnected intentionally

  reconnectAttempts++;
  const delay = Math.min(1000 * Math.pow(2, reconnectAttempts - 1), 30_000);
  reconnectTimer = setTimeout(() => {
    if (useChatStore.getState().agentId === agentId) {
      useChatStore.setState({ connecting: true });
      openSocket(agentId);
    }
  }, delay);
}

function handleServerMessage(msg: ServerMessage) {
  const store = useChatStore;
  if (isStoppedTail(stopped, msg)) return;

  switch (msg.type) {
    case "connected":
      if (msg.session_id) {
        store.setState({ sessionId: msg.session_id });
      }
      if (pending) void collect(pending);
      break;

    case "message": {
      const content = msg.content ?? "";
      if (pending && msg.session_id === pending.sessionId) {
        settle(pending, content);
        break;
      }
      if (repeatsCollected(store.getState().messages, collected, msg.session_id, content)) {
        collected = null;
        break;
      }
      store.setState((s) => ({
        messages: [...s.messages, assistantMessage(nextId(), content)],
        sessionId: msg.session_id ?? s.sessionId,
        isStreaming: false,
        isTyping: false,
      }));
      break;
    }

    case "typing":
      callStream = [];
      store.setState({ isTyping: true });
      break;

    // A file a tool delivered mid-turn. It lands as its own message, ahead of
    // an answer still streaming so that answer's deltas keep reaching it.
    case "image": {
      const attachments = msg.attachments ?? [];
      if (attachments.length === 0) break;
      store.setState((s) => {
        const sessionId = msg.session_id ?? s.sessionId ?? undefined;
        return {
          messages: insertBeforeStreaming(
            s.messages,
            attachmentMessage(nextId(), msg.content ?? "", attachments, sessionId),
          ),
          sessionId: sessionId ?? null,
        };
      });
      break;
    }

    case "stream_start": {
      const streamMsg: ChatMessage = {
        id: nextId(),
        role: "assistant",
        content: "",
        timestamp: new Date().toISOString(),
        status: "streaming",
        toolCalls: [],
      };
      callStream = [...callStream, streamMsg.id];
      store.setState((s) => ({
        messages: [...s.messages, streamMsg],
        sessionId: msg.session_id ?? s.sessionId,
        isStreaming: true,
        isTyping: false,
      }));
      break;
    }

    case "stream_delta":
      store.setState((s) => {
        const msgs = [...s.messages];
        const last = msgs[msgs.length - 1];
        if (last && last.status === "streaming") {
          msgs[msgs.length - 1] = { ...last, content: last.content + (msg.content ?? "") };
        }
        return { messages: msgs };
      });
      break;

    // The model call's streamed text is void; a replacement stream may follow.
    case "stream_discard": {
      const discarded = callStream;
      callStream = [];
      store.setState((s) => ({
        messages: discardCall(s.messages, discarded),
        isStreaming: false,
        isTyping: true,
      }));
      break;
    }

    // A stream that outlived a dropped socket reached this page with a gap;
    // the answer saved to the session replaces what arrived of it.
    case "stream_end":
      if (pending && msg.session_id === pending.sessionId) void collect(pending);
      store.setState((s) => {
        const msgs = [...s.messages];
        const last = msgs[msgs.length - 1];
        if (last && last.status === "streaming") {
          msgs[msgs.length - 1] = { ...last, status: "complete" };
        }
        return { messages: msgs, isStreaming: false };
      });
      break;

    // Deterministic code citations for the turn — delivered once, AFTER the
    // answer (stream_end or "message"). Attach them to the last completed
    // assistant message (the just-finished answer). Guarded: an error-only turn
    // leaves no assistant message to attach to.
    case "code_citations":
      store.setState((s) => {
        const citations = msg.code_citations;
        if (!citations || citations.length === 0) return {};
        return { messages: attachToLastAnswer(s.messages, { codeCitations: citations }) };
      });
      break;

    case "route":
      store.setState((s) => {
        const routeLine = msg.route;
        return routeLine ? { messages: attachToLastAnswer(s.messages, { routeLine }) } : {};
      });
      break;

    case "connect_prompt": {
      const prompt = msg.connect_prompt;
      if (!prompt) break;
      store.setState((s) => ({
        messages: [
          ...closeStreaming(s.messages),
          {
            id: nextId(),
            role: "system",
            content: prompt.reason,
            timestamp: new Date().toISOString(),
            status: "complete",
            toolCalls: [],
            connectPrompt: prompt,
          },
        ],
        sessionId: msg.session_id ?? s.sessionId,
        isStreaming: false,
      }));
      break;
    }

    case "tool_start":
      store.setState((s) => ({
        messages: updateToolCalls(s.messages, toolStarted(msg.content ?? "tool")),
      }));
      break;

    case "tool_end":
      store.setState((s) => ({ messages: updateToolCalls(s.messages, toolEnded) }));
      break;

    case "compact_start":
      store.setState({ isCompacting: true });
      break;

    case "compact_end":
      store.setState({ isCompacting: false });
      break;

    case "context_update":
      store.setState({
        contextTokens: msg.context_tokens ?? null,
        contextMax: msg.context_max ?? null,
      });
      break;

    case "error": {
      if (msg.session_id && msg.session_id === pending?.sessionId) pending = null;
      const errMsg: ChatMessage = {
        id: nextId(),
        role: "system",
        content: msg.error ?? msg.content ?? "An error occurred",
        timestamp: new Date().toISOString(),
        status: "error",
        toolCalls: [],
      };
      store.setState((s) => ({
        messages: [...s.messages, errMsg],
        isStreaming: false,
        isTyping: false,
      }));
      break;
    }

    case "pong":
      break;

    default:
      break;
  }
}

/** Every so often, and for as long as a resumed coding round may take,
 *  the session is read for an answer the page may have missed. */
const COLLECT_EVERY_MS = 15_000;
const COLLECT_FOR_MS = 30 * 60_000;
let collecting: PendingAnswer | null = null;

function awaitAnswer() {
  const { messages, sessionId } = useChatStore.getState();
  const question = outstandingQuestion(messages);
  if (!pending && question && sessionId) pending = { question, sessionId };
}

function settle(answered: PendingAnswer, content: string) {
  pending = null;
  collected = { sessionId: answered.sessionId, content };
  useChatStore.setState((s) => ({
    messages: withAnswer(s.messages, answered.question, assistantMessage(nextId(), content)),
    sessionId: answered.sessionId,
    isStreaming: false,
    isTyping: false,
  }));
}

function giveUp() {
  pending = null;
  useChatStore.setState((s) => ({
    messages: [
      ...s.messages.map((m) => (m.status === "streaming" ? { ...m, status: "complete" as const } : m)),
      {
        id: nextId(),
        role: "system",
        content: "No answer came back after the connection dropped. Send your message again if you still need it.",
        timestamp: new Date().toISOString(),
        status: "error",
        toolCalls: [],
      },
    ],
    isStreaming: false,
    isTyping: false,
  }));
}

async function collect(awaited: PendingAnswer) {
  if (collecting === awaited) return;
  collecting = awaited;
  const deadline = Date.now() + COLLECT_FOR_MS;
  try {
    while (pending === awaited) {
      const answer = await sessionsApi
        .transcript(awaited.sessionId, { limit: 200 })
        .then(({ entries }) => answerIn(entries, awaited.question))
        .catch(() => null);
      if (pending !== awaited) return;
      if (answer !== null) return settle(awaited, answer);
      if (Date.now() > deadline) return giveUp();
      await new Promise((resolve) => setTimeout(resolve, COLLECT_EVERY_MS));
    }
  } finally {
    if (collecting === awaited) collecting = null;
  }
}

/** MIME types the wire protocol expects as raw UTF-8, not base64. Mirrors
 *  `is_text_like_mime` in crates/common/src/session.rs — keep them in step. */
const TEXT_LIKE_MIME = new Set([
  "application/json",
  "application/xml",
  "application/yaml",
  "application/x-yaml",
  "application/toml",
  "application/javascript",
  "application/x-javascript",
  "application/typescript",
  "application/x-sh",
  "application/sql",
  "application/graphql",
  "application/x-httpd-php",
  "application/xhtml+xml",
  "application/ld+json",
  "application/x-ndjson",
]);

function isTextLikeMime(mime: string): boolean {
  return mime.startsWith("text/") || TEXT_LIKE_MIME.has(mime);
}

/** Wire shape of `attachments[]` both ways: inline files carry `data`,
 *  uploaded or staged ones carry `artifact_id`. */
export interface WireAttachment {
  filename: string;
  mime_type: string;
  data?: string;
  artifact_id?: string;
}

/** Files at or under this ride inline (base64) inside the WS message; larger
 *  ones POST to `/api/sessions/{id}/upload` first and ride by reference.
 *  Inline must stay well under the server's WS message cap (64 MiB) after
 *  base64's 4/3 inflation — and small enough that the persisted session row
 *  stays sane. */
const INLINE_ATTACHMENT_MAX_BYTES = 8 * 1024 * 1024;

/** Mirror of the server's per-file cap (`session_handlers::MAX_FILE_UPLOAD_BYTES`).
 *  Checked before uploading so a too-big file fails in milliseconds with a
 *  named limit instead of minutes later with a network error. */
const MAX_UPLOAD_FILE_BYTES = 1024 * 1024 * 1024;

interface UploadedFileInfo {
  filename: string;
  content_type: string;
  size_bytes: number;
  url: string;
  artifact_id?: string;
}

/** The anchor session id arrives in the `connected` handshake; a send that
 *  races it waits briefly instead of failing. (After `clearMessages` the id
 *  stays null until the next handshake or reply — the caller surfaces a
 *  "send a text message first" error for that case.) */
function awaitSessionId(timeoutMs = 5000): Promise<string | null> {
  const existing = useChatStore.getState().sessionId;
  if (existing) return Promise.resolve(existing);
  return new Promise((resolve) => {
    const deadline = Date.now() + timeoutMs;
    const timer = setInterval(() => {
      const sid = useChatStore.getState().sessionId;
      if (sid || Date.now() > deadline) {
        clearInterval(timer);
        resolve(sid);
      }
    }, 100);
  });
}

/** A human-readable reason out of an upload failure: the hub answers with
 *  `{"error": "..."}` JSON, and anything else (proxy HTML, bare statuses)
 *  must not land verbatim in a chat bubble. */
function apiErrorText(e: unknown): string {
  if (e instanceof ApiError) {
    try {
      const parsed = JSON.parse(e.message) as { error?: unknown };
      if (typeof parsed.error === "string") return parsed.error;
    } catch {
      // not JSON — fall through
    }
    if (e.status === 413) return "the file exceeds the server's upload limit";
    return `server responded with status ${e.status}`;
  }
  return e instanceof Error ? e.message : String(e);
}

/** Upload one large file to the session captured at send time and return the
 *  by-reference attachment entry for the chat message. `sessionId`/`agentId`
 *  are parameters, not store reads — the store can change mid-upload. */
async function uploadLargeFile(
  file: File,
  sessionId: string,
  agentId: string | null,
): Promise<WireAttachment> {
  const form = new FormData();
  form.append("file", file, file.name);
  const params = agentId ? `?agent_id=${encodeURIComponent(agentId)}` : "";
  let res: { files?: UploadedFileInfo[] };
  try {
    res = await gateway.postFormData<{ files?: UploadedFileInfo[] }>(
      `/api/sessions/${encodeURIComponent(sessionId)}/upload${params}`,
      form,
    );
  } catch (e) {
    throw new Error(`Upload of ${file.name} failed: ${apiErrorText(e)}`);
  }
  const uploaded = res.files?.[0];
  if (!uploaded?.artifact_id) {
    throw new Error(`Upload of ${file.name} did not return an artifact id.`);
  }
  return {
    filename: file.name,
    mime_type: file.type || "application/octet-stream",
    artifact_id: uploaded.artifact_id,
  };
}

/** Encode a file per the wire contract: `Attachment.data` is raw UTF-8 for
 *  text-like types and base64 for everything else. Base64-ing text anyway is
 *  not harmless — the server inlines the field verbatim into the model's
 *  context, so the model "reads" the base64 instead of the file. */
function fileToWireData(file: File): Promise<string> {
  const mime = file.type || "application/octet-stream";
  return isTextLikeMime(mime) ? file.text() : fileToBase64(file);
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result as string;
      // Strip the data:...;base64, prefix
      const base64 = result.split(",")[1] ?? result;
      resolve(base64);
    };
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
}
