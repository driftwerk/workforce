import { describe, expect, it } from "vitest";
import type { DecisionChoice } from "@/lib/api/workflows";
import { choiceVariant, decisionBadge, expiryText, inputProblem, waitingOnSentence } from "./decision";

describe("decision helpers", () => {
  it("maps choice styles to button variants", () => {
    expect(choiceVariant("primary")).toBe("default");
    expect(choiceVariant("danger")).toBe("destructive");
    expect(choiceVariant(null)).toBe("outline");
  });

  it("counts a decision step's buttons", () => {
    const button = { label: "Send", outcome: "send" };
    expect(decisionBadge({ deciders: "a@example.com", buttons: [button] })).toBe("decision · 1 button");
    expect(decisionBadge({ deciders: "a@example.com", buttons: [button, button] })).toBe("decision · 2 buttons");
  });

  it("words who a decision waits on", () => {
    expect(waitingOnSentence([])).toBe("nobody");
    expect(waitingOnSentence(["Alice"])).toBe("Alice");
    expect(waitingOnSentence(["Alice", "Bob"])).toBe("Alice or Bob");
    expect(waitingOnSentence(["Alice", "Bob", "Ana"])).toBe("Alice, Bob or Ana");
  });

  it("finds what is wrong with the text a choice asks for", () => {
    const revise: DecisionChoice = {
      label: "Revise",
      outcome: "revise",
      style: null,
      confirm: null,
      input: { label: "What should change?", placeholder: null, max_chars: 3, required: true },
    };
    expect(inputProblem("", revise)).toBe('Revise needs an answer to "What should change?".');
    expect(inputProblem(" \r\n\t", revise)).toBe('Revise needs an answer to "What should change?".');
    expect(inputProblem("éé€", revise)).toBeNull();
    expect(inputProblem(" a\r\nb ", revise)).toBeNull();
    expect(inputProblem("abcd", revise)).toBe("Revise takes at most 3 characters; this was 4.");
    const optional = { ...revise, input: { ...revise.input!, required: false } };
    expect(inputProblem("", optional)).toBeNull();
    expect(inputProblem("anything at all", { ...revise, input: null })).toBeNull();
  });

  it("words the expiry", () => {
    const now = new Date("2026-10-16T09:00:00Z");
    expect(expiryText(null, "Cancel", now)).toBeNull();
    expect(expiryText("2026-10-17T05:00:00Z", "Cancel", now)).toBe("Expires in 20h → Cancel");
    expect(expiryText("2026-10-16T09:30:00Z", null, now)).toBe("Expires in 30m");
    expect(expiryText("2026-10-20T09:00:00Z", "Cancel", now)).toBe("Expires in 4d → Cancel");
    expect(expiryText("2026-10-16T08:00:00Z", "Cancel", now)).toBe("Expiring now → Cancel");
  });
});
