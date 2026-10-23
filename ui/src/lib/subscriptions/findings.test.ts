import { describe, expect, it } from "vitest";
import { ApiError } from "@/lib/api-client";
import { attachFindings, blocksSave, refusalFindings, unattachedFindings, type Finding } from "./findings";

const refused: Finding = {
  severity: "refused",
  spec: "codex:gpt-5.6-sol",
  message: "\"codex:gpt-5.6-sol\" routes to codex, which runs only on the remote worker",
};
const warning: Finding = {
  severity: "warning",
  spec: "chatgpt:gpt-5.5",
  message: "\"chatgpt:gpt-5.5\": the install has chatgpt_direct off; calls on this route are refused",
};
const logins: Finding = { severity: "refused", spec: "[llm.logins]", message: "[llm.logins] tiers must name at least one tier" };

describe("findings", () => {
  it("finding attaches to field whose value is the spec", () => {
    const findings = [refused, warning, logins];
    expect(attachFindings(findings, " chatgpt:gpt-5.5 ")).toEqual([warning]);
    expect(attachFindings(findings, "codex:gpt-5.6-sol")).toEqual([refused]);
    expect(attachFindings(findings, "")).toEqual([]);
    expect(unattachedFindings(findings, ["chatgpt:gpt-5.5", "codex:gpt-5.6-sol"])).toEqual([logins]);
  });

  it("refused finding blocks save", () => {
    expect(blocksSave([warning])).toBe(false);
    expect(blocksSave([warning, refused])).toBe(true);
    expect(blocksSave([])).toBe(false);
  });

  it("refusal carries the findings of a refused write", () => {
    const body = JSON.stringify({ error: refused.message, findings: [refused, warning] });
    expect(refusalFindings(new ApiError(400, body))).toEqual([refused, warning]);
    expect(refusalFindings(new ApiError(400, JSON.stringify({ error: "not TOML" })))).toEqual([]);
    expect(refusalFindings(new ApiError(502, "<html>bad gateway</html>"))).toEqual([]);
    expect(refusalFindings("not an error")).toEqual([]);
  });
});
