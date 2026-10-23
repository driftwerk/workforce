import { describe, expect, it } from "vitest";
import type { CatalogInstallOutcome, CatalogSkillView } from "@/lib/api/skills";
import { CATALOG_ACTION_LABEL, catalogAction, installNote } from "./catalog";

const LIVE = "live on every agent now";
const STAGED = "written from outside the hub, so a running hub loads it when it next starts";

const skill = (overrides: Partial<CatalogSkillView>): CatalogSkillView => ({
  catalog: "example",
  name: "hello-world",
  kind: "external",
  description: "Says hello",
  installed: false,
  ...overrides,
});

const outcome = (overrides: Partial<CatalogInstallOutcome>): CatalogInstallOutcome => ({
  catalog: "example",
  skill: "hello-world",
  kind: "external",
  installed: true,
  active: true,
  activation: LIVE,
  ...overrides,
});

describe("catalogAction", () => {
  it("installs a skill that is not installed", () => {
    expect(catalogAction(skill({ version: "1.0.0" }))).toBe("install");
  });

  it("updates an installed skill whose catalog version differs, in either direction", () => {
    const behind = skill({ installed: true, version: "2.0.0", installed_version: "1.0.0" });
    const ahead = skill({ installed: true, version: "1.0.0", installed_version: "2.0.0" });
    expect(catalogAction(behind)).toBe("update");
    expect(catalogAction(ahead)).toBe("update");
  });

  it("reinstalls an installed skill at the same or an unknown version", () => {
    for (const versions of [
      { version: "1.0.0", installed_version: "1.0.0" },
      { version: "1.0.0" },
      { installed_version: "1.0.0" },
      {},
    ]) {
      expect(catalogAction(skill({ installed: true, ...versions })), JSON.stringify(versions)).toBe(
        "reinstall",
      );
    }
  });

  it("labels every action", () => {
    expect(CATALOG_ACTION_LABEL).toEqual({
      install: "Install",
      update: "Update",
      reinstall: "Reinstall",
    });
  });
});

describe("installNote", () => {
  it("reports a first install with its activation", () => {
    expect(installNote(outcome({ version: "1.0.0" }))).toBe(`Installed — ${LIVE}`);
  });

  it("reports the versions an update moved between", () => {
    const updated = outcome({ replaced: true, previous_version: "1.0.0", version: "2.0.0" });
    const downgraded = outcome({ replaced: true, previous_version: "2.0.0", version: "1.0.0" });
    expect(installNote(updated)).toBe(`Updated from v1.0.0 to v2.0.0 — ${LIVE}`);
    expect(installNote(downgraded)).toBe(`Updated from v2.0.0 to v1.0.0 — ${LIVE}`);
  });

  it("reports a reinstall at the same or an unknown version", () => {
    const same = outcome({
      replaced: true,
      previous_version: "1.0.0",
      version: "1.0.0",
      active: false,
      activation: STAGED,
    });
    expect(installNote(same)).toBe(`Reinstalled — ${STAGED}`);
    expect(installNote(outcome({ replaced: true, version: "1.0.0" }))).toBe(`Reinstalled — ${LIVE}`);
  });
});
