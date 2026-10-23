import { describe, expect, it } from "vitest";
import {
  VISIBILITIES,
  canTransfer,
  definitionLabel,
  definitionReach,
  definitionVisibility,
  definitionWord,
  visibilityChoices,
  visibilityHelp,
  visibilityLabel,
} from "./access";

describe("visibility choices", () => {
  it("offers the three reaches, closed first", () => {
    expect(VISIBILITIES).toEqual(["private", "team", "everyone"]);
  });

  it("labels each one and says who it reaches", () => {
    expect(VISIBILITIES.map(visibilityLabel)).toEqual([
      "Private",
      "My teams",
      "Everyone",
    ]);
    expect(VISIBILITIES.map(visibilityHelp)).toEqual([
      "Only you and the people you share it with",
      "Anyone in a team with you can run it — nobody, until you join one",
      "Anyone on this install can run it",
    ]);
  });

  it("warns that the middle choice reaches nobody without a team", () => {
    expect(visibilityHelp("team")).toContain("until you join one");
  });
});

describe("visibilityChoices", () => {
  it("offers all three where there is an owner whose teams can be resolved", () => {
    expect(visibilityChoices(false)).toEqual(VISIBILITIES);
  });

  it("drops the middle choice for a resource nobody owns", () => {
    expect(visibilityChoices(true)).toEqual(["private", "everyone"]);
  });
});

describe("definitionVisibility", () => {
  it("reads the words a definition row stores", () => {
    expect(definitionVisibility("global")).toBe("everyone");
    expect(definitionVisibility("team")).toBe("team");
    expect(definitionVisibility("private")).toBe("private");
  });

  it("fails closed on an absent or unrecognised word", () => {
    expect(definitionVisibility(undefined)).toBe("private");
    expect(definitionVisibility("everyone")).toBe("private");
  });

  it("round-trips every choice through the word a save stores", () => {
    expect(VISIBILITIES.map(definitionWord)).toEqual([
      "private",
      "team",
      "global",
    ]);
    for (const reach of VISIBILITIES) {
      expect(definitionVisibility(definitionWord(reach))).toBe(reach);
    }
  });
});

describe("definitionReach", () => {
  it("badges only the reaches that widen the private default", () => {
    expect(definitionReach("global")).toBe("Global");
    expect(definitionReach("team")).toBe("My teams");
    expect(definitionReach("private")).toBeUndefined();
    expect(definitionReach(undefined)).toBeUndefined();
  });

  it("names the default too, where a card always shows one", () => {
    expect(definitionLabel("private")).toBe("Private");
    expect(definitionLabel(undefined)).toBe("Private");
    expect(definitionLabel("team")).toBe("My teams");
    expect(definitionLabel("global")).toBe("Global");
  });
});

describe("canTransfer", () => {
  const owner = { id: "owner-1", role: "member" };
  const admin = { id: "admin-1", role: "system_admin" };
  const other = { id: "other-1", role: "member" };

  it("offers the owner their own resource, and an administrator anyone's", () => {
    expect(canTransfer(owner, "owner-1")).toBe(true);
    expect(canTransfer(admin, "owner-1")).toBe(true);
  });

  it("withholds it from everyone else, a manage grantee included", () => {
    expect(canTransfer(other, "owner-1")).toBe(false);
    expect(canTransfer(null, "owner-1")).toBe(false);
    expect(canTransfer(undefined, "owner-1")).toBe(false);
  });

  it("withholds it where nobody owns the resource, the administrator too", () => {
    expect(canTransfer(owner, null)).toBe(false);
    expect(canTransfer(owner, undefined)).toBe(false);
    expect(canTransfer(admin, null)).toBe(false);
  });
});
