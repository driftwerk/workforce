import type { ChatMessage } from "@/stores/chat-store";
import type { TranscriptEntry } from "@/lib/api/sessions";

/** A question whose answer was still coming when the page lost its socket,
 *  and the session that answer is saved to. */
export interface PendingAnswer {
  question: string;
  sessionId: string;
}

/** Directives the hub saves as user rows, such as the one resuming a turn
 *  cut short by a restart; none is a question the user asked. */
const DIRECTIVE = "[SYSTEM:";

/** The question still waiting on its answer: the last one sent, unless an
 *  answer or an error has come since. A file a tool delivered is neither. */
export function outstandingQuestion(messages: ChatMessage[]): string | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]!;
    if (m.attachments) continue;
    if (m.role === "user") return m.status === "complete" && m.content ? m.content : null;
    if (m.role === "system" || m.status === "complete") return null;
  }
  return null;
}

/** The answer a session holds to `question`, if it was the last thing the
 *  user asked there. */
export function answerIn(entries: TranscriptEntry[], question: string): string | null {
  let answer: string | null = null;
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i]!;
    if (e.role === "assistant") answer ??= e.content;
    if (e.role === "user" && !e.content.startsWith(DIRECTIVE)) {
      return e.content.trim() === question.trim() ? answer : null;
    }
  }
  return null;
}

/** The conversation with `answer` in place of whatever part of it had
 *  arrived after `question` before the socket went, its tools and any files
 *  it delivered kept. */
export function withAnswer(
  messages: ChatMessage[],
  question: string,
  answer: ChatMessage,
): ChatMessage[] {
  let asked = messages.length - 1;
  while (asked >= 0 && !(messages[asked]!.role === "user" && messages[asked]!.content === question)) {
    asked--;
  }
  const after = messages.slice(asked + 1);
  const toolCalls = after
    .filter((m) => m.role === "assistant")
    .flatMap((m) => m.toolCalls.map((t) => ({ ...t, active: false })));
  return [
    ...messages.slice(0, asked + 1),
    ...after.filter((m) => m.role !== "assistant" || m.attachments),
    { ...answer, toolCalls },
  ];
}

/** Whether a pushed answer repeats the one already collected from the
 *  session: both deliver the same saved answer, in either order. */
export function repeatsCollected(
  messages: ChatMessage[],
  collected: { sessionId: string; content: string } | null,
  sessionId: string | undefined,
  content: string,
): boolean {
  const last = messages[messages.length - 1];
  return (
    collected !== null &&
    collected.sessionId === sessionId &&
    collected.content === content &&
    last?.role === "assistant" &&
    last.content === content
  );
}
