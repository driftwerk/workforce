import { describe, expect, it } from "vitest";
import type { AgentEnvironments } from "@/lib/api/environments";
import {
  toBody,
  withDefault,
  withEnabled,
  withPromoted,
  withEnvironment,
  withMcp,
  withoutEnvironment,
  withoutMcp,
  withoutSkill,
  withoutSkillInstance,
  withSkill,
} from "./environments";

const view: AgentEnvironments = {
  allowed: true,
  enabled: true,
  default: "work",
  environments: [
    {
      name: "work",
      default: true,
      draft: false,
      description: "Day job",
      skills: { linear: ["driftwerk", "fabrikam-crm"] },
      mcp: { harbor: "prod" },
      warnings: ["`metabase` is not bound"],
    },
    { name: "empty", default: false, draft: false, description: null, skills: {}, mcp: {}, warnings: [] },
    {
      name: "proposed",
      default: false,
      draft: true,
      description: null,
      skills: { linear: ["fabrikam-crm"] },
      mcp: {},
      warnings: [],
    },
  ],
  options: {
    skills: [{ name: "linear", instances: ["driftwerk", "fabrikam-crm"], default: "driftwerk" }],
    mcp_workspaces: [{ name: "harbor", envs: ["prod", "staging"] }],
  },
  etag: '"abc"',
};

describe("environment body helpers", () => {
  it("builds the PUT body from the view, leaving out empty fields", () => {
    expect(toBody(view)).toEqual({
      enabled: true,
      default: "work",
      defs: {
        work: { description: "Day job", skills: { linear: ["driftwerk", "fabrikam-crm"] }, mcp: { harbor: "prod" } },
        empty: {},
        proposed: { skills: { linear: ["fabrikam-crm"] }, draft: true },
      },
    });
  });

  it("keeps a draft a draft through any other edit", () => {
    const body = withMcp("proposed", "harbor", "staging")(withSkill("work", "linear", ["fabrikam-crm"])(toBody(view)));
    expect(body.defs.proposed).toEqual({ skills: { linear: ["fabrikam-crm"] }, mcp: { harbor: "staging" }, draft: true });
    expect(withoutSkillInstance("proposed", "linear", "fabrikam-crm")(toBody(view)).defs.proposed).toEqual({ draft: true });
  });

  it("approves a draft, keeping its bindings", () => {
    expect(withPromoted("proposed")(toBody(view)).defs.proposed).toEqual({ skills: { linear: ["fabrikam-crm"] } });
    expect(withPromoted("work")(toBody(view)).defs.work).toEqual(toBody(view).defs.work);
  });

  it("approves a draft it makes the default", () => {
    const body = withDefault("proposed")(toBody(view));
    expect(body.default).toBe("proposed");
    expect(body.defs.proposed).toEqual({ skills: { linear: ["fabrikam-crm"] } });
  });

  it("does not mutate the view", () => {
    const before = structuredClone(view);
    withSkill("work", "linear", ["fabrikam-crm"])(toBody(view));
    expect(view).toEqual(before);
  });

  it("switches the agent and picks or clears the default", () => {
    const body = toBody(view);
    expect(withEnabled(false)(body).enabled).toBe(false);
    expect(withDefault("empty")(body).default).toBe("empty");
    expect(withDefault(null)(body).default).toBeNull();
  });

  it("creates an environment with a trimmed, optional description", () => {
    const body = toBody(view);
    expect(withEnvironment("staging", "  ")(body).defs.staging).toEqual({});
    expect(withEnvironment("staging", " QA ")(body).defs.staging).toEqual({ description: "QA" });
  });

  it("deletes an environment and clears the default when it was the default", () => {
    const body = toBody(view);
    expect(withoutEnvironment("work")(body)).toEqual({
      enabled: true,
      default: null,
      defs: { empty: {}, proposed: { skills: { linear: ["fabrikam-crm"] }, draft: true } },
    });
    expect(withoutEnvironment("empty")(body).default).toBe("work");
  });

  it("binds a skill to ordered, distinct instances, replacing the old binding", () => {
    const body = toBody(view);
    expect(withSkill("work", "linear", ["fabrikam-crm", "driftwerk", "fabrikam-crm"])(body).defs.work!.skills).toEqual({
      linear: ["fabrikam-crm", "driftwerk"],
    });
    expect(withSkill("empty", "linear", ["driftwerk"])(body).defs.empty).toEqual({ skills: { linear: ["driftwerk"] } });
    expect(withSkill("work", "linear", [])(body).defs.work!.skills).toBeUndefined();
  });

  it("removes one instance, and the binding with its last instance", () => {
    const one = withoutSkillInstance("work", "linear", "driftwerk")(toBody(view));
    expect(one.defs.work!.skills).toEqual({ linear: ["fabrikam-crm"] });
    const none = withoutSkillInstance("work", "linear", "fabrikam-crm")(one);
    expect(none.defs.work).toEqual({ description: "Day job", mcp: { harbor: "prod" } });
    expect(withoutSkill("work", "linear")(toBody(view)).defs.work!.skills).toBeUndefined();
  });

  it("binds and unbinds an MCP workspace", () => {
    const body = toBody(view);
    expect(withMcp("work", "harbor", "staging")(body).defs.work!.mcp).toEqual({ harbor: "staging" });
    expect(withoutMcp("work", "harbor")(body).defs.work).toEqual({
      description: "Day job",
      skills: { linear: ["driftwerk", "fabrikam-crm"] },
    });
  });

  it("leaves the body alone for an environment it does not have", () => {
    const body = toBody(view);
    expect(withSkill("missing", "linear", ["driftwerk"])(body)).toBe(body);
  });
});
