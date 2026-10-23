import { X, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type {
  ButtonStyle,
  PlaybookButton,
  PlaybookDefinition,
  PlaybookStep,
} from "@/lib/api/playbooks";

interface ButtonEditorPanelProps {
  def: PlaybookDefinition;
  stepId: string;
  onChange: (def: PlaybookDefinition) => void;
  onClose: () => void;
}

const SELECT_CLS = "flex-1 min-w-0 rounded-md border px-2 py-1.5 text-xs bg-background";

/** The button's effect as a single select value: `goto:<id>` / `exit` / `finish`. */
function buttonTarget(b: PlaybookButton): string {
  if (b.exit) return "exit";
  if (b.finish) return "finish";
  return `goto:${b.goto ?? ""}`;
}

function applyTarget(b: PlaybookButton, value: string): PlaybookButton {
  if (value === "exit") return { ...b, goto: undefined, exit: true, finish: false };
  if (value === "finish") return { ...b, goto: undefined, exit: false, finish: true };
  return { ...b, goto: value.slice("goto:".length), exit: false, finish: false };
}

/**
 * Side panel for editing a step's model, custom buttons, and Continue toggle by
 * form. Edits rebuild the definition and hand it back via `onChange`, which the
 * page re-serialises to TOML — so the graph, code, and Slack preview all stay in
 * sync off the single source of truth.
 */
export function ButtonEditorPanel({ def, stepId, onChange, onClose }: ButtonEditorPanelProps) {
  const step = def.step.find((s) => s.id === stepId);
  if (!step) return null;

  const updateStep = (patch: Partial<PlaybookStep>) => {
    onChange({
      ...def,
      step: def.step.map((s) => (s.id === stepId ? { ...s, ...patch } : s)),
    });
  };
  const updateButton = (i: number, next: PlaybookButton) =>
    updateStep({ buttons: step.buttons.map((b, j) => (j === i ? next : b)) });
  const addButton = () =>
    updateStep({
      buttons: [
        ...step.buttons,
        { label: "Button", style: "default", goto: def.step[0]?.id ?? stepId },
      ],
    });
  const removeButton = (i: number) =>
    updateStep({ buttons: step.buttons.filter((_, j) => j !== i) });

  return (
    <div className="absolute right-3 top-3 z-20 w-80 max-h-[calc(100%-1.5rem)] overflow-y-auto rounded-lg border bg-card shadow-lg">
      <div className="flex items-center justify-between border-b px-3 py-2">
        <div className="min-w-0">
          <div className="text-xs font-semibold truncate">{step.title || "Step"}</div>
          <div className="text-[10px] text-muted-foreground font-mono truncate">{step.id}</div>
        </div>
        <Button variant="ghost" size="icon" className="h-6 w-6" onClick={onClose}>
          <X className="h-3.5 w-3.5" />
        </Button>
      </div>

      <div className="p-3 space-y-3">
        <p className="text-[10px] text-muted-foreground">
          Edits here rewrite the TOML (comments and formatting are normalised).
        </p>
        <label className="block space-y-1 text-xs">
          <span className="font-medium">Model</span>
          <Input
            value={step.model ?? ""}
            onChange={(e) => updateStep({ model: e.target.value || undefined })}
            placeholder="default — e.g. powerful or openrouter:anthropic:powerful"
            className="h-7 text-xs font-mono"
          />
        </label>
        <label className="flex items-center gap-2 text-xs">
          <input
            type="checkbox"
            checked={step.keepContinue}
            onChange={(e) => updateStep({ keepContinue: e.target.checked })}
          />
          Show the default <span className="font-medium">Continue</span> button
        </label>

        <div className="space-y-2">
          {step.buttons.length === 0 && (
            <p className="text-[11px] text-muted-foreground">
              No custom buttons yet — add one to branch from this step.
            </p>
          )}
          {step.buttons.map((b, i) => (
            <div key={i} className="rounded-md border p-2 space-y-1.5">
              <div className="flex items-center gap-1.5">
                <Input
                  value={b.label}
                  onChange={(e) => updateButton(i, { ...b, label: e.target.value })}
                  placeholder="Label (emoji ok)"
                  className="h-7 text-xs"
                />
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-7 w-7 shrink-0 text-destructive hover:text-destructive"
                  onClick={() => removeButton(i)}
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </Button>
              </div>
              <div className="flex gap-1.5">
                <select
                  className={SELECT_CLS}
                  value={b.style}
                  onChange={(e) => updateButton(i, { ...b, style: e.target.value as ButtonStyle })}
                >
                  <option value="default">Grey</option>
                  <option value="primary">Green</option>
                  <option value="danger">Red</option>
                </select>
                <select
                  className={SELECT_CLS}
                  value={buttonTarget(b)}
                  onChange={(e) => updateButton(i, applyTarget(b, e.target.value))}
                >
                  {def.step.map((s) => (
                    <option key={s.id} value={`goto:${s.id}`}>
                      → {s.id}
                    </option>
                  ))}
                  <option value="finish">✅ Finish</option>
                  <option value="exit">✕ Exit</option>
                </select>
              </div>
            </div>
          ))}
        </div>

        <Button variant="outline" size="sm" className="w-full h-7 text-xs" onClick={addButton}>
          <Plus className="h-3.5 w-3.5 mr-1" /> Add button
        </Button>

        {!step.keepContinue && step.buttons.length === 0 && (
          <p className="text-[11px] text-amber-600 dark:text-amber-500">
            ⚠ Continue is hidden and there are no buttons — this step has no way forward. Add a
            button or re-enable Continue.
          </p>
        )}
      </div>
    </div>
  );
}
