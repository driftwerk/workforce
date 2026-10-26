import { beforeEach, describe, expect, it, vi } from "vitest";

/** A scriptable stand-in for the browser WebSocket. */
class FakeWebSocket {
  static OPEN = 1;
  static CONNECTING = 0;
  static last: FakeWebSocket;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((e: { data: string }) => void) | null = null;

  constructor() {
    FakeWebSocket.last = this;
  }

  send() {}

  close() {}
}

vi.stubGlobal("WebSocket", FakeWebSocket);
vi.stubGlobal("window", { location: { protocol: "http:", host: "hub.test" } });
vi.stubGlobal("sessionStorage", { getItem: () => null, setItem: () => {}, removeItem: () => {} });

const { useChatStore } = await import("./chat-store");

/** Deliver server messages, in order, on the page's socket. */
function server(...messages: Record<string, unknown>[]) {
  for (const message of messages) {
    FakeWebSocket.last.onmessage?.({ data: JSON.stringify({ session_id: "s1", ...message }) });
  }
}

/** A streamed answer: its own `stream_start` and `text`, left open. */
const streamed = (text: string) => [
  { type: "stream_start" },
  { type: "stream_delta", content: text },
];

const answers = () =>
  useChatStore
    .getState()
    .messages.filter((m) => m.role === "assistant")
    .map((m) => m.content);

beforeEach(() => {
  useChatStore.getState().clearMessages();
  useChatStore.getState().connect("agent");
  server({ type: "connected" });
});

describe("stream_discard", () => {
  it("typing starts a new call", () => {
    server(
      { type: "typing" },
      ...streamed("Earlier call."),
      { type: "stream_end" },
      { type: "typing" },
      ...streamed("Abandoned"),
      { type: "stream_discard", content: "interrupted" },
    );
    expect(answers()).toEqual(["Earlier call."]);
  });

  it("takes back a segment its call already ended", () => {
    server(
      { type: "typing" },
      ...streamed("Before the tool call"),
      { type: "stream_end" },
      { type: "stream_discard", content: "interrupted" },
      ...streamed("The real answer."),
      { type: "stream_end" },
    );
    expect(answers()).toEqual(["The real answer."]);
  });

  it("a second discard takes back only the replacement", () => {
    server(
      { type: "typing" },
      ...streamed("First try"),
      { type: "stream_discard", content: "interrupted" },
      ...streamed("Second try"),
      { type: "stream_discard", content: "interrupted" },
      ...streamed("Third time lucky."),
      { type: "stream_end" },
    );
    expect(answers()).toEqual(["Third time lucky."]);
  });

  it("leaves the page waiting on the replacement", () => {
    server({ type: "typing" }, ...streamed("Abandoned"), { type: "stream_discard", content: "retry" });
    expect(useChatStore.getState()).toMatchObject({ isStreaming: false, isTyping: true });
  });

  it("never takes back the answer a stop kept", () => {
    server({ type: "typing" }, ...streamed("Kept where it stopped"));
    useChatStore.getState().cancel();
    useChatStore.getState().send("Next question");
    server({ type: "stream_discard", content: "interrupted" });
    expect(answers()).toEqual(["Kept where it stopped"]);
  });
});
