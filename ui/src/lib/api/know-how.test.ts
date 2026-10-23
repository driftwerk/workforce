import { beforeEach, describe, expect, it, vi } from "vitest";

const get = vi.fn(() => Promise.resolve({}));
const post = vi.fn(() => Promise.resolve({}));
const del = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { get, post, del } }));

const { knowHowApi } = await import("./know-how");

describe("knowHowApi", () => {
  beforeEach(() => {
    get.mockClear();
    post.mockClear();
    del.mockClear();
  });

  it("addresses an agent's entries with its id encoded", () => {
    knowHowApi.list("data & ops");
    expect(get).toHaveBeenCalledWith("/api/agents/data%20%26%20ops/know-how");
    knowHowApi.pin("data & ops", 7);
    expect(post).toHaveBeenCalledWith("/api/agents/data%20%26%20ops/know-how/7/pin");
    knowHowApi.retire("atlas", 7);
    expect(post).toHaveBeenLastCalledWith("/api/agents/atlas/know-how/7/retire");
    knowHowApi.remove("atlas", 7);
    expect(del).toHaveBeenCalledWith("/api/agents/atlas/know-how/7");
  });

  it("erases one identity on every agent", () => {
    knowHowApi.eraseIdentity("11111111-1111-1111-1111-111111111111");
    expect(del).toHaveBeenCalledWith(
      "/api/identities/11111111-1111-1111-1111-111111111111/know-how",
    );
  });
});
