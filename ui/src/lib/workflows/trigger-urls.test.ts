import { describe, expect, it } from "vitest";
import { triggerAddressList, triggerAddresses } from "./trigger-urls";

const MINTED = "https://hub.example.com/triggers/t.3f2c1e40";
const OWNER_DERIVED = "https://hub.example.com/triggers/alice/deploy";

describe("triggerAddresses", () => {
  it("hands out the minted address and shows the older one beside it", () => {
    expect(triggerAddresses({ trigger_id: "deploy", url: MINTED, legacy_url: OWNER_DERIVED })).toEqual({
      triggerId: "deploy",
      primary: MINTED,
      alsoAnswers: OWNER_DERIVED,
    });
  });

  it("promotes the older address where nothing was minted, and never labels it", () => {
    expect(triggerAddresses({ trigger_id: "deploy", legacy_url: OWNER_DERIVED })).toEqual({
      triggerId: "deploy",
      primary: OWNER_DERIVED,
      alsoAnswers: null,
    });
  });

  it("never says an address also answers when only the minted one does", () => {
    expect(triggerAddresses({ trigger_id: "deploy", url: MINTED }).alsoAnswers).toBeNull();
  });

  it("reports a trigger with no working address rather than hiding it", () => {
    expect(triggerAddresses({ trigger_id: "dup" })).toEqual({
      triggerId: "dup",
      primary: null,
      alsoAnswers: null,
    });
  });
});

describe("triggerAddressList", () => {
  it("is empty for a workflow with no HTTP trigger, so the tab stays hidden", () => {
    expect(triggerAddressList(undefined)).toEqual([]);
    expect(triggerAddressList([])).toEqual([]);
  });

  it("keeps every trigger, in order", () => {
    expect(
      triggerAddressList([
        { trigger_id: "a", url: MINTED },
        { trigger_id: "b", legacy_url: OWNER_DERIVED },
      ]).map((t) => t.triggerId),
    ).toEqual(["a", "b"]);
  });
});
