import { useQuery } from "@tanstack/react-query";
import { parse as parseToml } from "smol-toml";
import { skillsApi } from "@/lib/api/skills";

/** Skill directory that holds the Xero `[entities.*]` config. */
const XERO_SKILL_DIR = "xero-integration";

/**
 * Configured Xero entity names, read from the Xero skill's `config.toml`
 * (`[entities.<name>]` table keys). These drive the per-entity mapping columns.
 * Fails soft: an empty list (e.g. config absent or unparseable) lets the editor
 * fall back to manual entity entry.
 */
export function useXeroEntities() {
  return useQuery({
    queryKey: ["xero-entities"],
    queryFn: async (): Promise<string[]> => {
      const toml = await skillsApi.getConfig(XERO_SKILL_DIR);
      if (!toml || !toml.trim()) return [];
      const parsed = parseToml(toml) as { entities?: Record<string, unknown> };
      const entities = parsed.entities;
      if (!entities || typeof entities !== "object") return [];
      return Object.keys(entities);
    },
    staleTime: 60_000,
    // Config may be missing/unreadable — degrade gracefully, don't retry-storm.
    retry: false,
  });
}
