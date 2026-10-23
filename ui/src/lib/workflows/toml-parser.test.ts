import { describe, expect, it } from "vitest";
import { workflowDefToToml } from "./graph-to-toml";
import { parseWorkflowToml, tomlToGraph } from "./toml-parser";

const PACKAGED = `
[workflow]
id = "handover"
name = "Handover"

[[step]]
id = "extract"
name = "Extract"
task = "Extract the schema."
outputs = [
  { name = "tables", type = "list", description = "table names" },
  { name = "notes", required = false },
]

[[step]]
id = "tests"
name = "Tests"
task = "Write a test per table."
receives = ["extract"]
verify = { criteria = ["every table has a test"], corrections = 2 }
`;

describe("parseWorkflowToml", () => {
  it("keeps a step's package schema, receives and verify through a visual edit", () => {
    const { def, error } = parseWorkflowToml(PACKAGED);
    expect(error).toBeNull();

    const again = parseWorkflowToml(workflowDefToToml(def!)).def!;

    expect(again.step[0]?.outputs).toEqual([
      { name: "tables", type: "list", description: "table names" },
      { name: "notes", required: false, description: "" },
    ]);
    expect(again.step[1]).toMatchObject({
      receives: ["extract"],
      verify: { criteria: ["every table has a test"], corrections: 2 },
    });
  });
});

describe("workflow-level fields", () => {
  const SETTINGS = `
[workflow]
id = "release"
name = "Release"
description = "Ship a release"
version = "2.1"
agent = "builder"
delivery = "reports"
max_concurrent = 3
failure_report = """
Release {{ version }} stopped at {{ step }}: {{ error }}"""
pause_on_usage_cap = true
steerable_by_others = false

[workflow.requires]
github = ">=0.10.0"

[workflow.context]
version = { required = true }

[[step]]
id = "ship"
name = "Ship"
task = "Ship it."
exit = true
`;

  it("keeps the fields the editor does not type through a visual edit", () => {
    const { def, error } = parseWorkflowToml(SETTINGS);
    expect(error).toBeNull();

    const again = parseWorkflowToml(workflowDefToToml(def!));

    expect(again.error).toBeNull();
    expect(again.def!.workflow).toEqual(def!.workflow);
    expect(again.def!.workflow).toMatchObject({
      description: "Ship a release",
      version: "2.1",
      agent: "builder",
      delivery: "reports",
      failure_report: "Release {{ version }} stopped at {{ step }}: {{ error }}",
      pause_on_usage_cap: true,
      max_concurrent: 3,
      steerable_by_others: false,
      requires: { github: ">=0.10.0" },
      context: { version: { required: true } },
    });
  });

  it("leaves steering open when the definition says nothing", () => {
    const { def } = parseWorkflowToml(PACKAGED);
    expect(def!.workflow.steerable_by_others).toBeUndefined();
    expect(workflowDefToToml(def!)).not.toContain("steerable_by_others");
  });
});

describe("decision steps", () => {
  const DIGEST = `
[workflow]
id = "daily-digest-email"
name = "Daily digest email"

[[step]]
id = "draft"
name = "Draft the digest"
task = "Draft today's digest."

[[step]]
id = "review"
name = "Review the digest"
report_render = "template"
report = "Today's digest"
pause = "awaiting-approval"
branches = [{ when = "send", goto = "send_email" }]

[step.decision]
title = "Send today's digest?"
context = "Drafted from yesterday's figures"
channel = "dm"
deciders = "approver.one@example.com, team:reviewers"
allow_self_decision = false
expires_after = "20h"
on_expiry = "cancel"

[[step.decision.buttons]]
label = "Send"
outcome = "send"
style = "primary"
done = "Sent"
confirm = "Send this email now?"

[[step.decision.buttons]]
label = "Redraft"
outcome = "redraft"
goto = "draft"

[[step.decision.buttons]]
label = "Cancel"
outcome = "cancel"
style = "danger"
end = "cancel"

[[step.decision.buttons]]
label = "Escalate"
outcome = "escalate"
workflow = { id = "digest-escalation", payload = { subject = "Today's digest" } }
set = { escalated = "yes" }
end = "complete"

[[step.decision.buttons]]
label = "Notify"
outcome = "notify"
workflow = "digest-notice"
end = "complete"

[[step]]
id = "after"
name = "After"
task = "Never reached."

[[step]]
id = "send_email"
name = "Send"
task = "Send it."
exit = true
`;

  it("keeps a decision through a visual edit", () => {
    const { def, error } = parseWorkflowToml(DIGEST);
    expect(error).toBeNull();

    const again = parseWorkflowToml(workflowDefToToml(def!)).def!;

    expect(again.step[1]?.decision).toEqual(def!.step[1]?.decision);
    expect(again.step[1]?.decision?.allow_self_decision).toBe(false);
    expect(again.step[1]?.decision?.buttons.map((b) => b.outcome)).toEqual([
      "send",
      "redraft",
      "cancel",
      "escalate",
      "notify",
    ]);
    expect(again.step[1]?.decision?.buttons[3]?.workflow).toEqual({
      id: "digest-escalation",
      payload: { subject: "Today's digest" },
    });
    expect(again.step[1]?.decision?.buttons[4]?.workflow).toBe("digest-notice");
    expect(again.step[2]?.decision).toBeUndefined();
  });

  it("draws an edge for each goto button and no fall-through from a decision step", () => {
    const { edges } = tomlToGraph(parseWorkflowToml(DIGEST).def!);
    const fromReview = edges.filter((e) => e.source === "review");

    expect(fromReview.map((e) => [e.type, e.target, e.label])).toEqual([
      ["branchEdge", "send_email", "send"],
      ["decisionEdge", "draft", "Redraft"],
    ]);
    expect(edges.some((e) => e.id === "review-decision-redraft")).toBe(true);
    expect(edges.some((e) => e.source === "review" && e.target === "after")).toBe(false);
  });
});
