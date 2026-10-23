import type { PlaybookButton, PlaybookDefinition } from "@/lib/api/playbooks";
import { escapeTomlString, isMultiLine } from "@/lib/toml-util";

/** Emit a button's single effect line (`goto` / `exit` / `finish`). */
function buttonEffectLine(b: PlaybookButton): string {
  if (b.exit) return "  exit = true";
  if (b.finish) return "  finish = true";
  return `  goto = "${escapeTomlString(b.goto ?? "")}"`;
}

/**
 * Render the `prompt = …` line. Multi-line prompts use a TOML *literal* string
 * (`'''…'''`), which does no escape processing — so backslashes, regexes and
 * Windows paths in a prompt survive verbatim instead of breaking the parse. The
 * layout puts a newline right after the opener (TOML trims it) and none before
 * the closer, so the round-trip neither corrupts content nor accretes newlines.
 * A prompt that itself contains `'''` falls back to an escaped basic string.
 */
function promptLine(prompt: string): string {
  if (!isMultiLine(prompt)) {
    return `prompt = "${escapeTomlString(prompt)}"`;
  }
  if (!prompt.includes("'''")) {
    return `prompt = '''\n${prompt}'''`;
  }
  return `prompt = """\n${escapeTomlString(prompt)}"""`;
}

/**
 * Serialise a PlaybookDefinition back to TOML — the inverse of
 * `parsePlaybookToml`, used by the visual button editor to write edits back into
 * the source. Like the workflow serialiser, this regenerates the document, so it
 * normalises formatting and drops comments (the round-trip is structural).
 */
export function playbookDefToToml(def: PlaybookDefinition): string {
  const lines: string[] = [];

  lines.push("[playbook]");
  lines.push(`id = "${escapeTomlString(def.playbook.id)}"`);
  lines.push(`name = "${escapeTomlString(def.playbook.name)}"`);
  if (def.playbook.description) {
    lines.push(`description = "${escapeTomlString(def.playbook.description)}"`);
  }

  for (const step of def.step) {
    lines.push("");
    lines.push("[[step]]");
    lines.push(`id = "${escapeTomlString(step.id)}"`);
    lines.push(`title = "${escapeTomlString(step.title)}"`);
    if (step.artifact) {
      lines.push(`artifact = "${escapeTomlString(step.artifact)}"`);
    }
    if (step.model?.trim()) {
      lines.push(`model = "${escapeTomlString(step.model.trim())}"`);
    }
    // keep_continue defaults to true, so only emit the override.
    if (!step.keepContinue) {
      lines.push("keep_continue = false");
    }
    lines.push(promptLine(step.prompt));

    for (const button of step.buttons) {
      lines.push("");
      lines.push("  [[step.button]]");
      lines.push(`  label = "${escapeTomlString(button.label)}"`);
      if (button.style !== "default") {
        lines.push(`  style = "${button.style}"`);
      }
      lines.push(buttonEffectLine(button));
    }
  }

  lines.push("");
  return lines.join("\n");
}
