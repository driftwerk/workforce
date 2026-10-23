import { beforeEach, describe, expect, it, vi } from "vitest";

const get = vi.fn(() => Promise.resolve({}));
const put = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { get, put } }));

const { environmentsApi } = await import("./environments");

describe("environmentsApi", () => {
  beforeEach(() => {
    get.mockClear();
    put.mockClear();
  });

  it("reads one agent's environments with its id encoded", () => {
    environmentsApi.get("data & ops");
    expect(get).toHaveBeenCalledWith("/api/agents/data%20%26%20ops/environments");
  });

  it("replaces them only as of the ETag it read", () => {
    const body = { enabled: true, default: "work", defs: { work: { skills: { linear: ["driftwerk"] } } } };
    environmentsApi.put("data & ops", '"abc"', body);
    expect(put).toHaveBeenCalledWith("/api/agents/data%20%26%20ops/environments", body, {
      "If-Match": '"abc"',
    });
  });
});
