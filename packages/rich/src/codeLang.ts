// Which language a fenced code block is in, from what the agent wrote after the
// backticks. Only the languages listed here are highlighted; each grammar is a
// lazy chunk of its own, reached through a literal `import()` so the host's
// bundler can split it — a template-literal path would not be split, because
// the package build leaves `@shikijs/langs` a bare import for the host.

import type { LanguageRegistration } from "@shikijs/core";

type GrammarModule = { default: LanguageRegistration[] };

export interface CodeLang {
  /** The Shiki grammar that highlights it, or `null` to draw it plain. */
  grammar: string | null;
  /** What the block's header calls it. */
  label: string;
  /** A unified diff: its lines are coloured as added, removed or context, from
   *  their text alone, with no grammar to wait for. */
  diff?: boolean;
}

interface Entry {
  label: string;
  aliases?: string[];
  load?: () => Promise<GrammarModule>;
}

// Keyed by Shiki's own grammar name: that is what `codeToTokens` is asked for.
const TABLE: Record<string, Entry> = {
  typescript: { label: "TypeScript", aliases: ["ts", "mts", "cts"], load: () => import("@shikijs/langs/typescript") },
  tsx: { label: "TSX", load: () => import("@shikijs/langs/tsx") },
  javascript: {
    label: "JavaScript",
    aliases: ["js", "mjs", "cjs", "node"],
    load: () => import("@shikijs/langs/javascript"),
  },
  jsx: { label: "JSX", load: () => import("@shikijs/langs/jsx") },
  json: { label: "JSON", load: () => import("@shikijs/langs/json") },
  jsonc: { label: "JSONC", aliases: ["json5"], load: () => import("@shikijs/langs/jsonc") },
  yaml: { label: "YAML", aliases: ["yml"], load: () => import("@shikijs/langs/yaml") },
  toml: { label: "TOML", load: () => import("@shikijs/langs/toml") },
  rust: { label: "Rust", aliases: ["rs"], load: () => import("@shikijs/langs/rust") },
  python: { label: "Python", aliases: ["py", "py3", "python3"], load: () => import("@shikijs/langs/python") },
  go: { label: "Go", aliases: ["golang"], load: () => import("@shikijs/langs/go") },
  sql: {
    label: "SQL",
    aliases: ["postgres", "postgresql", "psql", "pgsql", "mysql", "sqlite", "plpgsql"],
    load: () => import("@shikijs/langs/sql"),
  },
  shellscript: {
    label: "Shell",
    aliases: ["bash", "sh", "shell", "zsh", "ksh"],
    load: () => import("@shikijs/langs/shellscript"),
  },
  shellsession: {
    label: "Console",
    aliases: ["console", "shell-session", "terminal"],
    load: () => import("@shikijs/langs/shellsession"),
  },
  powershell: { label: "PowerShell", aliases: ["ps1", "pwsh"], load: () => import("@shikijs/langs/powershell") },
  docker: { label: "Dockerfile", aliases: ["dockerfile", "containerfile"], load: () => import("@shikijs/langs/docker") },
  make: { label: "Makefile", aliases: ["makefile", "mk"], load: () => import("@shikijs/langs/make") },
  html: { label: "HTML", aliases: ["htm"], load: () => import("@shikijs/langs/html") },
  xml: { label: "XML", aliases: ["svg", "plist"], load: () => import("@shikijs/langs/xml") },
  css: { label: "CSS", load: () => import("@shikijs/langs/css") },
  scss: { label: "SCSS", load: () => import("@shikijs/langs/scss") },
  markdown: { label: "Markdown", aliases: ["md"], load: () => import("@shikijs/langs/markdown") },
  graphql: { label: "GraphQL", aliases: ["gql"], load: () => import("@shikijs/langs/graphql") },
  java: { label: "Java", load: () => import("@shikijs/langs/java") },
  kotlin: { label: "Kotlin", aliases: ["kt", "kts"], load: () => import("@shikijs/langs/kotlin") },
  swift: { label: "Swift", load: () => import("@shikijs/langs/swift") },
  c: { label: "C", aliases: ["h"], load: () => import("@shikijs/langs/c") },
  cpp: { label: "C++", aliases: ["c++", "cc", "cxx", "hpp"], load: () => import("@shikijs/langs/cpp") },
  csharp: { label: "C#", aliases: ["cs", "c#"], load: () => import("@shikijs/langs/csharp") },
  ruby: { label: "Ruby", aliases: ["rb"], load: () => import("@shikijs/langs/ruby") },
  php: { label: "PHP", load: () => import("@shikijs/langs/php") },
  lua: { label: "Lua", load: () => import("@shikijs/langs/lua") },
  r: { label: "R", load: () => import("@shikijs/langs/r") },
  scala: { label: "Scala", load: () => import("@shikijs/langs/scala") },
  elixir: { label: "Elixir", aliases: ["ex", "exs"], load: () => import("@shikijs/langs/elixir") },
  haskell: { label: "Haskell", aliases: ["hs"], load: () => import("@shikijs/langs/haskell") },
  hcl: { label: "HCL", load: () => import("@shikijs/langs/hcl") },
  terraform: { label: "Terraform", aliases: ["tf", "tfvars"], load: () => import("@shikijs/langs/terraform") },
  proto: { label: "Protobuf", aliases: ["protobuf"], load: () => import("@shikijs/langs/proto") },
  ini: { label: "INI", aliases: ["cfg"], load: () => import("@shikijs/langs/ini") },
  dotenv: { label: ".env", aliases: ["env"], load: () => import("@shikijs/langs/dotenv") },
  nginx: { label: "nginx", load: () => import("@shikijs/langs/nginx") },
  vue: { label: "Vue", load: () => import("@shikijs/langs/vue") },
  svelte: { label: "Svelte", load: () => import("@shikijs/langs/svelte") },
  dart: { label: "Dart", load: () => import("@shikijs/langs/dart") },
  zig: { label: "Zig", load: () => import("@shikijs/langs/zig") },
  nix: { label: "Nix", load: () => import("@shikijs/langs/nix") },
  perl: { label: "Perl", aliases: ["pl"], load: () => import("@shikijs/langs/perl") },
  http: { label: "HTTP", load: () => import("@shikijs/langs/http") },
  diff: { label: "Diff", aliases: ["patch", "udiff"] },
};

// A Map, not an object: a fence such as ```constructor must not resolve to a
// prototype member.
const BY_NAME = new Map<string, string>();
for (const [name, entry] of Object.entries(TABLE)) {
  BY_NAME.set(name, name);
  for (const alias of entry.aliases ?? []) BY_NAME.set(alias, name);
}

/** The grammar loader for a name {@link resolveLang} returned. */
export function grammarLoader(grammar: string): (() => Promise<GrammarModule>) | undefined {
  return BY_NAME.get(grammar) === grammar ? TABLE[grammar]?.load : undefined;
}

/** Every highlighted grammar, by Shiki name. */
export const GRAMMARS: readonly string[] = Object.keys(TABLE).filter((name) => TABLE[name]?.load);

const PLAIN: CodeLang = { grammar: null, label: "Text" };
const LABEL_MAX = 24;

/** The language of a block from its fence word (`null` for a bare fence) and,
 *  failing that, from a cheap and reliable look at its text: only a block that
 *  parses as a JSON object or array is called JSON. An unknown fence word keeps
 *  its own name in the header and is drawn plain. */
export function resolveLang(fence: string | null, text: string): CodeLang {
  if (fence) {
    const name = BY_NAME.get(fence.toLowerCase());
    if (name === "diff") return { grammar: null, label: TABLE.diff!.label, diff: true };
    if (name) return { grammar: name, label: TABLE[name]!.label };
    return { grammar: null, label: fence.length > LABEL_MAX ? fence.slice(0, LABEL_MAX - 1) + "…" : fence };
  }
  return looksLikeJson(text) ? { grammar: "json", label: "JSON" } : PLAIN;
}

const JSON_GUESS_MAX = 64 * 1024;

function looksLikeJson(text: string): boolean {
  const t = text.trim();
  if (t.length < 2 || t.length > JSON_GUESS_MAX) return false;
  const open = t[0];
  const close = t[t.length - 1];
  if (!((open === "{" && close === "}") || (open === "[" && close === "]"))) return false;
  try {
    JSON.parse(t);
    return true;
  } catch {
    return false;
  }
}

export type DiffLine = "add" | "del" | "hunk" | "meta" | null;

/** How a line of a unified diff is coloured, from its first characters. */
export function diffLine(line: string): DiffLine {
  if (line.startsWith("+++ ") || line.startsWith("--- ")) return "meta";
  if (line.startsWith("diff ") || line.startsWith("index ")) return "meta";
  if (line.startsWith("@@")) return "hunk";
  if (line.startsWith("+")) return "add";
  if (line.startsWith("-")) return "del";
  return null;
}
