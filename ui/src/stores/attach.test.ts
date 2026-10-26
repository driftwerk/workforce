import { describe, expect, it } from "vitest";
import type { ChatMessage } from "./chat-store";
import {
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

const msg = (id: string, role: ChatMessage["role"], status: ChatMessage["status"]): ChatMessage => ({
  id,
  role,
  content: id,
  timestamp: "",
  status,
  toolCalls: [],
});

describe("attachToLastAnswer", () => {
  it("route and citations attach to last completed answer", () => {
    const citation = {
      repo: "r", owner: "o", repo_name: "n", commit: "c", path: "p",
      start_line: 1, end_line: 2, kind: "k", symbol: "s",
    };
    const messages = [
      msg("first", "assistant", "complete"),
      msg("question", "user", "complete"),
      msg("answer", "assistant", "complete"),
      msg("notice", "system", "complete"),
    ];
    const cited = attachToLastAnswer(messages, { codeCitations: [citation] });
    const routed = attachToLastAnswer(cited, { routeLine: "Claude Code · sonnet · Alice's plan" });

    expect(routed[2]).toMatchObject({
      codeCitations: [citation],
      routeLine: "Claude Code · sonnet · Alice's plan",
    });
    expect(routed[0]).toBe(messages[0]);
    expect(routed[3]).toBe(messages[3]);
    const streaming = [msg("partial", "assistant", "streaming")];
    expect(attachToLastAnswer(streaming, { routeLine: "x" })).toBe(streaming);
  });
});

describe("closeStreaming", () => {
  it("closes an answer cut off mid-stream and drops one that streamed nothing", () => {
    const partial = { ...msg("partial", "assistant", "streaming"), content: "half an ans" };
    const empty = { ...msg("empty", "assistant", "streaming"), content: "" };
    const done = msg("done", "assistant", "complete");
    expect(closeStreaming([done, partial])).toEqual([done, { ...partial, status: "complete" }]);
    expect(closeStreaming([done, empty])).toEqual([done]);
  });
});

describe("closeStopped", () => {
  it("keeps the stopped answer, drops an empty one, and ends every running tool", () => {
    const partial = {
      ...msg("partial", "assistant", "streaming"),
      content: "half",
      toolCalls: [{ name: "coding.session", active: true }],
    };
    const empty = { ...msg("empty", "assistant", "streaming"), content: "" };
    const earlier = { ...msg("earlier", "assistant", "complete"), toolCalls: [{ name: "exec.run", active: true }] };
    expect(closeStopped([earlier, partial, empty])).toEqual([
      { ...earlier, toolCalls: [{ name: "exec.run", active: false }] },
      { ...partial, status: "complete", toolCalls: [{ name: "coding.session", active: false }] },
    ]);
  });
});

describe("isStoppedTail", () => {
  it("drops the stopped turn's events but lets pushes and errors through", () => {
    expect(isStoppedTail("s1", { type: "stream_delta", session_id: "s1" })).toBe(true);
    expect(isStoppedTail("s1", { type: "tool_start", session_id: "s1" })).toBe(true);
    expect(isStoppedTail("s1", { type: "stream_discard", session_id: "s1" })).toBe(true);
    expect(isStoppedTail("s1", { type: "message", session_id: "s1" })).toBe(false);
    expect(isStoppedTail("s1", { type: "error", session_id: "s1" })).toBe(false);
    expect(isStoppedTail("s1", { type: "image", session_id: "s1" })).toBe(false);
    expect(isStoppedTail("s1", { type: "stream_delta", session_id: "s2" })).toBe(false);
    expect(isStoppedTail(null, { type: "stream_delta", session_id: "s1" })).toBe(false);
  });
});

describe("updateToolCalls", () => {
  it("delivered_file_mid_tool_keeps_tool_events_on_the_answer", () => {
    const answer = msg("answer", "assistant", "complete");
    const started = updateToolCalls([answer], toolStarted("chart"));
    const file = attachmentMessage("file", "Pro leads.", [], "s1");
    const ended = updateToolCalls([...started, file], toolEnded);
    expect(ended[0]!.toolCalls).toEqual([{ name: "chart", active: false }]);
    expect(ended[1]!.toolCalls).toEqual([]);
    const next = updateToolCalls(ended, toolStarted("email.send"));
    expect(next[0]!.toolCalls.map((t) => t.name)).toEqual(["chart", "email.send"]);
    expect(next[1]!.toolCalls).toEqual([]);
  });

  it("connect_prompt_mid_tool_keeps_tool_events_on_the_answer", () => {
    const answer = msg("answer", "assistant", "streaming");
    const started = updateToolCalls([answer], toolStarted("chatgpt.login"));
    const card: ChatMessage = {
      ...msg("card", "system", "complete"),
      connectPrompt: { kind: "chatgpt", agent_id: "allan", reason: "sign in", private: true, resumes: false },
    };
    const ended = updateToolCalls([...started, card], toolEnded);

    expect(ended[0]!.toolCalls).toEqual([{ name: "chatgpt.login", active: false }]);
    expect(ended[1]).toBe(card);
  });
});

describe("attachmentMessage", () => {
  it("is a complete answer carrying the files, the caption and the file's session", () => {
    const files = [{ filename: "q3.png", mime_type: "image/png", data: "AAAA" }];
    expect(attachmentMessage("m1", "Pro leads.", files, "s1")).toMatchObject({
      id: "m1",
      role: "assistant",
      status: "complete",
      content: "Pro leads.",
      attachments: files,
      sessionId: "s1",
      toolCalls: [],
    });
  });
});

describe("insertBeforeStreaming", () => {
  const delivered = attachmentMessage("file", "", [], "s1");

  it("appends when nothing is streaming", () => {
    const messages = [msg("q", "user", "complete"), msg("a", "assistant", "complete")];
    expect(insertBeforeStreaming(messages, delivered).map((m) => m.id)).toEqual(["q", "a", "file"]);
  });

  it("keeps a streaming answer last so its deltas still reach it", () => {
    const messages = [msg("q", "user", "complete"), msg("a", "assistant", "streaming")];
    expect(insertBeforeStreaming(messages, delivered).map((m) => m.id)).toEqual(["q", "file", "a"]);
  });
});

describe("discardCall", () => {
  it("stream_discard removes the call's bubbles only", () => {
    const messages = [
      msg("earlier", "assistant", "complete"),
      msg("q", "user", "complete"),
      msg("narration", "assistant", "complete"),
      msg("draft", "assistant", "streaming"),
    ];
    expect(discardCall(messages, ["narration", "draft"]).map((m) => m.id)).toEqual(["earlier", "q"]);
  });

  it("leaves everything when the call streamed nothing", () => {
    const messages = [msg("q", "user", "complete"), msg("a", "assistant", "complete")];
    expect(discardCall(messages, [])).toEqual(messages);
  });
});
