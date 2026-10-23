import { describe, expect, it } from "vitest";
import { ownerNote, readinessNote, readinessWarning, toolsNote } from "./owned";

describe("ownerNote", () => {
  it("names a skill with no owner as the org's", () => {
    expect(ownerNote({ owner: null, mine: false })).toBe("installed for the org");
  });

  it("calls the signed-in user's own skill theirs", () => {
    expect(ownerNote({ owner: "alice", mine: true }, "Alice")).toBe("yours");
  });

  it("names another user's skill by its owner", () => {
    expect(ownerNote({ owner: "bob", mine: false }, "Bob")).toBe("owned by Bob");
  });

  it("falls back when the owner cannot be named", () => {
    expect(ownerNote({ owner: "carol", mine: false })).toBe("owned by another user");
    expect(ownerNote({ owner: "carol", mine: false }, "")).toBe("owned by another user");
  });
});

describe("toolsNote", () => {
  it("counts tools in the singular and plural", () => {
    expect(toolsNote(0)).toBe("0 tools");
    expect(toolsNote(1)).toBe("1 tool");
    expect(toolsNote(2)).toBe("2 tools");
  });
});

describe("readiness", () => {
  const ready = { available: true, gating: "all requirements met" };
  const unmet = {
    available: false,
    gating:
      "2 requirement(s) not met:\n  ✗ bin:jq — Binary 'jq' not found on PATH\n" +
      "  ✗ env:API_TOKEN — Environment variable 'API_TOKEN' is not set",
  };

  it("says an available skill is ready, with nothing to warn about", () => {
    expect(readinessNote(ready)).toBe("ready to use");
    expect(readinessWarning(ready)).toBeUndefined();
  });

  it("keeps the note short and gives every unmet requirement as the warning", () => {
    expect(readinessNote(unmet)).toBe("not ready");
    expect(readinessWarning(unmet)).toBe(unmet.gating);
  });
});
