import type {
  AgentEnvironments,
  EnvironmentDef,
  EnvironmentsBody,
  McpBindings,
  SkillBindings,
} from "@/lib/api/environments";

export type Change = (body: EnvironmentsBody) => EnvironmentsBody;

const without = <V>(record: Record<string, V>, key: string): Record<string, V> =>
  Object.fromEntries(Object.entries(record).filter(([k]) => k !== key));

interface DefParts {
  description?: string | null;
  skills?: SkillBindings;
  mcp?: McpBindings;
  draft?: boolean;
}

const def = ({ description, skills = {}, mcp = {}, draft }: DefParts): EnvironmentDef => ({
  ...(description ? { description } : {}),
  ...(Object.keys(skills).length > 0 ? { skills } : {}),
  ...(Object.keys(mcp).length > 0 ? { mcp } : {}),
  ...(draft ? { draft: true } : {}),
});

type CurrentDef = DefParts & { skills: SkillBindings; mcp: McpBindings };

const updateDef =
  (env: string, change: (current: CurrentDef) => DefParts): Change =>
  (body) => {
    const current = body.defs[env];
    if (!current) return body;
    const parts = { ...current, skills: current.skills ?? {}, mcp: current.mcp ?? {} };
    return { ...body, defs: { ...body.defs, [env]: def({ ...parts, ...change(parts) }) } };
  };

export const toBody = (view: AgentEnvironments): EnvironmentsBody => ({
  enabled: view.enabled,
  default: view.default,
  defs: Object.fromEntries(view.environments.map((env) => [env.name, def(env)])),
});

export const withEnabled =
  (enabled: boolean): Change =>
  (body) => ({ ...body, enabled });

/** Approves a draft, so sessions can use it. */
export const withPromoted = (env: string): Change => updateDef(env, () => ({ draft: false }));

/** Makes `env` the default, approving it if it is a draft, since a draft can never be the default. */
export const withDefault =
  (env: string | null): Change =>
  (body) => ({ ...(env ? withPromoted(env)(body) : body), default: env });

export const withEnvironment =
  (env: string, description: string): Change =>
  (body) => ({ ...body, defs: { ...body.defs, [env]: def({ description: description.trim() }) } });

export const withoutEnvironment =
  (env: string): Change =>
  (body) => ({
    ...body,
    default: body.default === env ? null : body.default,
    defs: without(body.defs, env),
  });

/** Binds `skill` to `instances` in order, replacing any binding it had; the first instance is its default. */
export const withSkill = (env: string, skill: string, instances: string[]): Change =>
  updateDef(env, ({ skills }) => {
    const set = [...new Set(instances)];
    return { skills: set.length > 0 ? { ...skills, [skill]: set } : without(skills, skill) };
  });

export const withoutSkill = (env: string, skill: string): Change =>
  updateDef(env, ({ skills }) => ({ skills: without(skills, skill) }));

/** Drops one instance from a binding, and the binding itself once no instance is left. */
export const withoutSkillInstance = (env: string, skill: string, instance: string): Change =>
  updateDef(env, ({ skills }) => {
    const rest = (skills[skill] ?? []).filter((i) => i !== instance);
    return { skills: rest.length > 0 ? { ...skills, [skill]: rest } : without(skills, skill) };
  });

export const withMcp = (env: string, workspace: string, variant: string): Change =>
  updateDef(env, ({ mcp }) => ({ mcp: { ...mcp, [workspace]: variant } }));

export const withoutMcp = (env: string, workspace: string): Change =>
  updateDef(env, ({ mcp }) => ({ mcp: without(mcp, workspace) }));
