import { beforeEach, describe, expect, it, vi } from "vitest";

const get = vi.fn(() => Promise.resolve({}));
const post = vi.fn(() => Promise.resolve({}));
vi.mock("../api-client", () => ({ gateway: { get, post } }));

const { pairingApi } = await import("./pairing");

describe("pairingApi browser sign-in", () => {
  beforeEach(() => {
    get.mockClear();
    post.mockClear();
  });

  it("reads the waiting device by code", () => {
    pairingApi.devicePending("ABCD 2345");
    expect(get).toHaveBeenCalledWith("/api/pair/device/pending/ABCD%202345");
  });

  it("approves into the signed-in person by posting only the code", () => {
    pairingApi.approveSelf("ABCD2345EFGH");
    expect(post).toHaveBeenCalledWith("/api/pair/device/approve-self", { code: "ABCD2345EFGH" });
  });

  it("rejects by code", () => {
    pairingApi.rejectSelf("ABCD2345EFGH");
    expect(post).toHaveBeenCalledWith("/api/pair/device/reject-self", { code: "ABCD2345EFGH" });
  });

  it("lets an admin approve into a chosen person with the code and identity id", () => {
    const person = "11111111-1111-1111-1111-111111111111";
    pairingApi.approveInto("ABCD2345EFGH", person);
    expect(post).toHaveBeenCalledWith("/api/pair/device/approve", {
      code: "ABCD2345EFGH",
      identity_id: person,
    });
  });
});
