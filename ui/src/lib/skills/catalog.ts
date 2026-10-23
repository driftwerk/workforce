import type { CatalogInstallOutcome, CatalogSkillView } from "@/lib/api/skills";

export type CatalogAction = "install" | "update" | "reinstall";

export const CATALOG_ACTION_LABEL: Record<CatalogAction, string> = {
  install: "Install",
  update: "Update",
  reinstall: "Reinstall",
};

function versionChanges(from?: string, to?: string): boolean {
  return !!from && !!to && from !== to;
}

/** What installing a catalog skill would do: a first install, a version change, or a fresh copy. */
export function catalogAction(skill: CatalogSkillView): CatalogAction {
  if (!skill.installed) return "install";
  return versionChanges(skill.installed_version, skill.version) ? "update" : "reinstall";
}

/** The note shown under a catalog skill once an install returns. */
export function installNote(outcome: CatalogInstallOutcome): string {
  const { replaced, previous_version: previous, version } = outcome;
  const what = !replaced
    ? "Installed"
    : versionChanges(previous, version)
      ? `Updated from v${previous} to v${version}`
      : "Reinstalled";
  return `${what} — ${outcome.activation}`;
}
