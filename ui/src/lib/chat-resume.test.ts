import { describe, expect, it } from "vitest";
import type { ChatMessage } from "@/stores/chat-store";
import type { TranscriptEntry } from "@/lib/api/sessions";
import { answerIn, outstandingQuestion, repeatsCollected, withAnswer } from "./chat-resume";

const msg = (
  role: ChatMessage["role"],
  content: string,
  status: ChatMessage["status"] = "complete",
  tools: string[] = [],
): ChatMessage => ({
  id: content,
  role,
  content,
  timestamp: "",
  status,
  toolCalls: tools.map((name) => ({ name, active: true })),
});

const entry = (role: string, content: string): TranscriptEntry => ({
  id: content,
  session_id: "s1",
  role,
  content,
});

const RESUME = "[SYSTEM: Your previous response was interrupted by a server restart.]";

describe("outstandingQuestion", () => {
  it("is the last question sent while its answer is still streaming or not begun", () => {
    expect(outstandingQuestion([msg("user", "q1")])).toBe("q1");
    expect(outstandingQuestion([msg("user", "q1"), msg("assistant", "half", "streaming")])).toBe("q1");
  });

  it("is nothing once answered or failed, or for a message not sent yet", () => {
    expect(outstandingQuestion([msg("user", "q1"), msg("assistant", "a1")])).toBeNull();
    expect(outstandingQuestion([msg("user", "q1"), msg("system", "boom", "error")])).toBeNull();
    expect(outstandingQuestion([msg("user", "q1", "error")])).toBeNull();
    expect(outstandingQuestion([msg("user", "q1", "sending")])).toBeNull();
    expect(outstandingQuestion([])).toBeNull();
  });
});

describe("outstandingQuestion with a delivered file", () => {
  it("still waits on the answer after a tool delivered a file", () => {
    const file = { ...msg("assistant", ""), attachments: [{ filename: "q3.png", mime_type: "image/png" }] };
    expect(outstandingQuestion([msg("user", "q1"), file])).toBe("q1");
    expect(outstandingQuestion([msg("user", "q1"), msg("assistant", "half", "streaming"), file])).toBe("q1");
    expect(outstandingQuestion([msg("user", "q1"), msg("assistant", "a1"), file])).toBeNull();
  });
});

describe("answerIn", () => {
  it("is the answer a resumed turn saved after the question", () => {
    const entries = [
      entry("user", "q1"),
      entry("tool", "ran"),
      entry("user", RESUME),
      entry("tool", "collected"),
      entry("assistant", "a1"),
    ];
    expect(answerIn(entries, "q1")).toBe("a1");
  });

  it("is nothing while the question is unanswered or another was asked since", () => {
    expect(answerIn([entry("assistant", "old"), entry("user", "q1")], "q1")).toBeNull();
    expect(answerIn([entry("user", "q1"), entry("assistant", "a1"), entry("user", "q2")], "q1")).toBeNull();
    expect(answerIn([], "q1")).toBeNull();
  });

  it("does not take the answer to a question it only begins", () => {
    expect(answerIn([entry("user", "yes, do it"), entry("assistant", "done")], "yes")).toBeNull();
  });
});

describe("withAnswer", () => {
  it("replaces what had arrived of the answer, keeping its tools settled", () => {
    const got = withAnswer(
      [msg("user", "q1"), msg("assistant", "half", "streaming", ["coding.session"])],
      "q1",
      msg("assistant", "a1"),
    );
    expect(got.map((m) => m.content)).toEqual(["q1", "a1"]);
    expect(got[1]!.toolCalls).toEqual([{ name: "coding.session", active: false }]);
  });

  it("keeps a file delivered while the answer was coming", () => {
    const file = { ...msg("assistant", ""), attachments: [{ filename: "q3.png", mime_type: "image/png" }] };
    const got = withAnswer(
      [msg("user", "q1"), file, msg("assistant", "half", "streaming", ["chart"])],
      "q1",
      msg("assistant", "a1"),
    );
    expect(got.map((m) => m.content)).toEqual(["q1", "", "a1"]);
    expect(got[1]!.attachments).toHaveLength(1);
    expect(got[2]!.toolCalls).toEqual([{ name: "chart", active: false }]);
  });

  it("replaces a gapped stream that ended, and follows a question nothing answered", () => {
    const gapped = withAnswer([msg("user", "q1"), msg("assistant", "ha")], "q1", msg("assistant", "a1"));
    expect(gapped.map((m) => m.content)).toEqual(["q1", "a1"]);
    const bare = withAnswer([msg("assistant", "a0"), msg("user", "q1")], "q1", msg("assistant", "a1"));
    expect(bare.map((m) => m.content)).toEqual(["a0", "q1", "a1"]);
  });
});

describe("repeatsCollected", () => {
  const settled = [msg("user", "q1"), msg("assistant", "a1")];
  const collected = { sessionId: "s1", content: "a1" };

  it("recognises the push of an answer already read from its session", () => {
    expect(repeatsCollected(settled, collected, "s1", "a1")).toBe(true);
  });

  it("lets every other message through", () => {
    expect(repeatsCollected(settled, null, "s1", "a1")).toBe(false);
    expect(repeatsCollected(settled, collected, "s2", "a1")).toBe(false);
    expect(repeatsCollected(settled, collected, "s1", "a reminder")).toBe(false);
    expect(repeatsCollected([...settled, msg("user", "q2")], collected, "s1", "a1")).toBe(false);
  });
});
