import { beforeEach, describe, expect, it, vi } from "vitest";

const put = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { put } }));

const { ownerApi } = await import("./shares");

/** What the transfer dialog puts on the wire. The keep-access choice belongs
 *  in this one request: while the outgoing owner still owns the resource they
 *  cannot be their own grantee, so a share sent beforehand is refused — and
 *  the share route replaces the whole grantee set, so one that did land would
 *  take every other grantee with it. */
describe("ownerApi.set", () => {
  beforeEach(() => put.mockClear());

  it("carries the keep-manage choice in the transfer itself", () => {
    ownerApi.set("workflows", "triage", "bob", "alice", true);
    expect(put).toHaveBeenCalledWith("/api/workflows/triage/owner?owner=alice", {
      owner: "bob",
      keep_manage: true,
    });
  });

  it("keeps nothing unless asked, and asks nothing of a skill's owner", () => {
    ownerApi.set("wasm-skills", "hello-world", "bob", null, false);
    expect(put).toHaveBeenCalledWith("/api/wasm-skills/hello-world/owner", {
      owner: "bob",
      keep_manage: false,
    });
  });
});
