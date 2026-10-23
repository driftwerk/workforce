import { Code2 } from "lucide-react";
import type { CodeCitation } from "@/stores/chat-store";

const BASE_CLASSES =
  "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-xs font-medium align-baseline max-w-full";

/** Repo-relative basename for a compact label (`vault.rs`, not the full path). */
function basename(path: string): string {
  const i = path.lastIndexOf("/");
  return i >= 0 ? path.slice(i + 1) : path;
}

/** `vault.rs:377-401`, or `vault.rs:377` for a single-line span. */
function locationLabel(c: CodeCitation): string {
  const file = basename(c.path);
  return c.end_line > c.start_line
    ? `${file}:${c.start_line}-${c.end_line}`
    : `${file}:${c.start_line}`;
}

/**
 * GitHub permalink to the exact grounded line range, pinned to the commit the
 * source was read at. Returns `null` when the citation has no GitHub owner —
 * the chip then renders as non-clickable text (the source location is still
 * shown).
 */
function permalink(c: CodeCitation): string | null {
  // Need owner, repo, AND the commit the body was read at — a missing commit
  // would yield a `/blob//path` 404, so fall back to non-link text instead.
  if (!c.owner || !c.repo_name || !c.commit) return null;
  return `https://github.com/${c.owner}/${c.repo_name}/blob/${c.commit}/${c.path}#L${c.start_line}-L${c.end_line}`;
}

/**
 * A deterministic code-citation chip: `kind symbol · file:start-end`. The
 * `code_context` injector produces these from the exact files/line-ranges it
 * grounded the answer in — independent of the model's inline prose — so the
 * grounding is verifiable. Clicking opens the source at GitHub.
 */
export function CodeCitationChip({ citation }: { citation: CodeCitation }) {
  const label = `${citation.kind} ${citation.symbol} · ${locationLabel(citation)}`;
  const title = `${citation.symbol} — ${citation.repo}/${citation.path}:${citation.start_line}-${citation.end_line}`;
  const href = permalink(citation);

  if (!href) {
    return (
      <span
        className={`${BASE_CLASSES} bg-muted text-muted-foreground`}
        title={title}
      >
        <Code2 className="h-3 w-3 shrink-0" />
        <span className="truncate">{label}</span>
      </span>
    );
  }

  return (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className={`${BASE_CLASSES} bg-blue-500/10 hover:bg-blue-500/20 text-blue-400 no-underline hover:no-underline`}
      title={title}
    >
      <Code2 className="h-3 w-3 shrink-0" />
      <span className="truncate">{label}</span>
    </a>
  );
}
