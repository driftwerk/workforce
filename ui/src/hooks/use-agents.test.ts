import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { invalidateAgentConfig } from "./use-agents";

describe("invalidateAgentConfig", () => {
  it("a saved or restored config refreshes everything read from that agent's config.toml", () => {
    const qc = new QueryClient();
    for (const key of [
      ["agent-config", "allan"],
      ["agent-config-revisions", "allan"],
      ["agent-logins", "allan"],
      ["agent-logins", "sarah"],
      ["agent-environments", "allan"],
      ["agent-environments", "sarah"],
    ]) {
      qc.setQueryData(key, {});
    }
    invalidateAgentConfig(qc, "allan");
    const stale = (key: string[]) => qc.getQueryState(key)?.isInvalidated;
    expect(stale(["agent-config", "allan"])).toBe(true);
    expect(stale(["agent-config-revisions", "allan"])).toBe(true);
    expect(stale(["agent-logins", "allan"])).toBe(true);
    expect(stale(["agent-logins", "sarah"])).toBe(false);
    expect(stale(["agent-environments", "allan"])).toBe(true);
    expect(stale(["agent-environments", "sarah"])).toBe(false);
  });
});
