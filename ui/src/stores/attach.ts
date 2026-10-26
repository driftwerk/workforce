import type { ChatMessage, ToolCall, WireAttachment } from "./chat-store";

/** `messages` with `patch` applied to the last completed assistant message —
 *  the answer that just finished — or as they were when there is none. */
export function attachToLastAnswer(
  messages: ChatMessage[],
  patch: Partial<ChatMessage>,
): ChatMessage[] {
  const at = messages.reduceRight(
    (found, m, i) => (found < 0 && m.role === "assistant" && m.status === "complete" ? i : found),
    -1,
  );
  return at < 0 ? messages : messages.map((m, i) => (i === at ? { ...m, ...patch } : m));
}

/** `messages` with an answer still streaming closed where it stopped, or
 *  dropped when it streamed nothing. */
export function closeStreaming(messages: ChatMessage[]): ChatMessage[] {
  return messages.flatMap((m) => {
    if (m.status !== "streaming") return [m];
    return m.content || m.toolCalls.length > 0 ? [{ ...m, status: "complete" as const }] : [];
  });
}

/** `messages` with the turn the user stopped closed: its answer kept where it
 *  stopped and its tool calls ended, since none of their ends will come. */
export function closeStopped(messages: ChatMessage[]): ChatMessage[] {
  return closeStreaming(messages).map((m) =>
    m.toolCalls.some((call) => call.active)
      ? { ...m, toolCalls: m.toolCalls.map((call) => ({ ...call, active: false })) }
      : m,
  );
}

/** Events a running turn emits — what a stopped turn may still have on the
 *  wire. Pushes, errors and delivered files are not among them, so they
 *  still land: a file a tool finished is kept, as it is on Slack. */
const TURN_EVENTS = new Set([
  "typing",
  "stream_start",
  "stream_delta",
  "stream_end",
  "stream_discard",
  "tool_start",
  "tool_end",
  "compact_start",
  "compact_end",
  "code_citations",
  "route",
]);

/** Whether `event` is the tail of the turn the user stopped in session
 *  `stopped`. */
export function isStoppedTail(
  stopped: string | null,
  event: { type: string; session_id?: string },
): boolean {
  return stopped !== null && event.session_id === stopped && TURN_EVENTS.has(event.type);
}

/** `messages` with the tool calls `update` gives the last message a tool
 *  event belongs to: never a connect card, which a sign-in started mid-tool
 *  puts after the answer the tool runs in, and never a delivered file, which
 *  a tool puts there the same way. `null` leaves them as they were. */
export function updateToolCalls(
  messages: ChatMessage[],
  update: (message: ChatMessage) => ToolCall[] | null,
): ChatMessage[] {
  const at = messages.reduceRight(
    (found, m, i) => (found < 0 && !m.connectPrompt && !m.attachments ? i : found),
    -1,
  );
  const toolCalls = at < 0 ? null : update(messages[at]!);
  return toolCalls ? messages.map((m, i) => (i === at ? { ...m, toolCalls } : m)) : messages;
}

/** A tool call started on `message`, when it is an answer under way or done. */
export function toolStarted(name: string) {
  return (message: ChatMessage): ToolCall[] | null =>
    message.status === "streaming" || message.status === "complete"
      ? [...message.toolCalls, { name, active: true }]
      : null;
}

/** `message`'s last active tool call, ended. */
export function toolEnded(message: ChatMessage): ToolCall[] | null {
  const at = message.toolCalls.reduceRight((found, call, i) => (found < 0 && call.active ? i : found), -1);
  return at < 0 ? null : message.toolCalls.map((call, i) => (i === at ? { ...call, active: false } : call));
}

/** A complete answer with `content`, as a push or a collected answer lands. */
export function assistantMessage(id: string, content: string): ChatMessage {
  return {
    id,
    role: "assistant",
    content,
    timestamp: new Date().toISOString(),
    status: "complete",
    toolCalls: [],
  };
}

/** The message a delivered file shows as: an answer carrying the file, with
 *  the delivery caption as its text and the session the file is served from. */
export function attachmentMessage(
  id: string,
  caption: string,
  attachments: WireAttachment[],
  sessionId: string | undefined,
): ChatMessage {
  return { ...assistantMessage(id, caption), attachments, sessionId };
}

/** `messages` without the answers a discarded model call streamed. */
export function discardCall(messages: ChatMessage[], streamed: readonly string[]): ChatMessage[] {
  return messages.filter((m) => !streamed.includes(m.id));
}

/** `messages` with `message` appended — ahead of an answer still streaming,
 *  which stays last so its deltas keep landing on it. */
export function insertBeforeStreaming(messages: ChatMessage[], message: ChatMessage): ChatMessage[] {
  const last = messages[messages.length - 1];
  return last && last.status === "streaming"
    ? [...messages.slice(0, -1), message, last]
    : [...messages, message];
}
