import { useState } from "react";
import { useNavigate } from "react-router";
import { Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useSaveGlobalSkill } from "@/hooks/use-skills";

interface VaultEntry {
  path: string;
  description: string;
  required: boolean;
}

interface SkillCreationWizardProps {
  open: boolean;
  onClose: () => void;
}

export function SkillCreationWizard({ open, onClose }: SkillCreationWizardProps) {
  const navigate = useNavigate();
  const saveMutation = useSaveGlobalSkill();
  const [step, setStep] = useState(0);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [version, setVersion] = useState("1.0.0");
  const [author, setAuthor] = useState("");

  const [bins, setBins] = useState("");
  const [envVars, setEnvVars] = useState("");
  const [vaultEntries, setVaultEntries] = useState<VaultEntry[]>([]);
  const [body, setBody] = useState("");

  const reset = () => {
    setStep(0);
    setName("");
    setDescription("");
    setVersion("1.0.0");
    setAuthor("");
    setBins("");
    setEnvVars("");
    setVaultEntries([]);
    setBody("");
  };

  const handleClose = () => {
    reset();
    onClose();
  };

  const kebabName = name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");

  const generateSkillMd = () => {
    const lines: string[] = ["---"];
    lines.push(`name: ${kebabName}`);
    lines.push(`version: ${version}`);
    lines.push(`description: ${description}`);
    if (author) lines.push(`author: ${author}`);

    const binsArr = bins.split(",").map((b) => b.trim()).filter(Boolean);
    const envArr = envVars.split(",").map((e) => e.trim()).filter(Boolean);
    const hasRequires = binsArr.length > 0 || envArr.length > 0 || vaultEntries.length > 0;

    if (hasRequires) {
      lines.push("requires:");
      if (binsArr.length > 0) {
        lines.push(`  bins: [${binsArr.join(", ")}]`);
      }
      if (envArr.length > 0) {
        lines.push(`  env: [${envArr.join(", ")}]`);
      }
      if (vaultEntries.length > 0) {
        lines.push("  vault:");
        for (const v of vaultEntries) {
          lines.push(`    - path: ${v.path}`);
          lines.push(`      description: "${v.description}"`);
          lines.push(`      required: ${v.required}`);
        }
      }
    }

    lines.push("---");
    lines.push("");
    lines.push(body || `# ${name}\n\nInstructions for using this skill.`);
    return lines.join("\n");
  };

  const addVaultEntry = () => {
    setVaultEntries([...vaultEntries, { path: `skills/${kebabName}/`, description: "", required: true }]);
  };

  const updateVaultEntry = (idx: number, field: keyof VaultEntry, value: string | boolean) => {
    const updated = [...vaultEntries];
    updated[idx] = { ...updated[idx]!, [field]: value };
    setVaultEntries(updated);
  };

  const removeVaultEntry = (idx: number) => {
    setVaultEntries(vaultEntries.filter((_, i) => i !== idx));
  };

  const canProceed = () => {
    if (step === 0) return kebabName.length > 0 && description.length > 0;
    return true;
  };

  const handleCreate = () => {
    const content = generateSkillMd();
    saveMutation.mutate(
      { name: kebabName, content },
      {
        onSuccess: () => {
          handleClose();
          navigate(`/skills/${kebabName}`);
        },
      },
    );
  };

  const STEPS = ["Basics", "Requirements", "Instructions", "Review"];

  return (
    <Dialog open={open} onOpenChange={(o) => !o && handleClose()}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Create New Skill</DialogTitle>
          <DialogDescription>
            Step {step + 1} of {STEPS.length}: {STEPS[step]}
          </DialogDescription>
        </DialogHeader>

        {/* Step indicator */}
        <div className="flex gap-1">
          {STEPS.map((_, i) => (
            <div key={i} className={`h-1 flex-1 rounded-full ${i <= step ? "bg-primary" : "bg-muted"}`} />
          ))}
        </div>

        <div className="space-y-4 py-2">
          {step === 0 && (
            <>
              <div className="space-y-2">
                <label htmlFor="name" className="text-sm font-medium">Skill Name</label>
                <Input id="name" placeholder="my-skill" value={name} onChange={(e) => setName(e.target.value)} />
                {name && <p className="text-xs text-muted-foreground">Directory: <code>{kebabName}</code></p>}
              </div>
              <div className="space-y-2">
                <label htmlFor="desc" className="text-sm font-medium">Description</label>
                <Input id="desc" placeholder="What this skill does" value={description} onChange={(e) => setDescription(e.target.value)} />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <label htmlFor="version" className="text-sm font-medium">Version</label>
                  <Input id="version" value={version} onChange={(e) => setVersion(e.target.value)} />
                </div>
                <div className="space-y-2">
                  <label htmlFor="author" className="text-sm font-medium">Author</label>
                  <Input id="author" placeholder="optional" value={author} onChange={(e) => setAuthor(e.target.value)} />
                </div>
              </div>
            </>
          )}

          {step === 1 && (
            <>
              <div className="space-y-2">
                <label htmlFor="bins" className="text-sm font-medium">Required Binaries</label>
                <Input id="bins" placeholder="curl, jq (comma-separated)" value={bins} onChange={(e) => setBins(e.target.value)} />
              </div>
              <div className="space-y-2">
                <label htmlFor="env" className="text-sm font-medium">Required Environment Variables</label>
                <Input id="env" placeholder="API_KEY (comma-separated)" value={envVars} onChange={(e) => setEnvVars(e.target.value)} />
              </div>
              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <span className="text-sm font-medium">Vault Secrets</span>
                  <Button variant="outline" size="sm" onClick={addVaultEntry}>
                    <Plus className="h-3 w-3 mr-1" /> Add
                  </Button>
                </div>
                {vaultEntries.length === 0 && (
                  <p className="text-xs text-muted-foreground">No vault secrets required. Add one if this skill needs API keys or tokens.</p>
                )}
                {vaultEntries.map((entry, i) => (
                  <div key={i} className="space-y-2 rounded-md border p-3">
                    <div className="flex items-center gap-2">
                      <Input
                        placeholder="skills/my-skill/token"
                        value={entry.path}
                        onChange={(e) => updateVaultEntry(i, "path", e.target.value)}
                        className="font-mono text-xs"
                      />
                      <Button variant="ghost" size="icon" onClick={() => removeVaultEntry(i)}>
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                    <Input
                      placeholder="Description (e.g., API token)"
                      value={entry.description}
                      onChange={(e) => updateVaultEntry(i, "description", e.target.value)}
                    />
                    <label className="flex items-center gap-2 cursor-pointer">
                      <input
                        type="checkbox"
                        checked={entry.required}
                        onChange={(e) => updateVaultEntry(i, "required", e.target.checked)}
                        className="rounded"
                      />
                      <span className="text-xs text-muted-foreground">Required</span>
                    </label>
                  </div>
                ))}
              </div>
            </>
          )}

          {step === 2 && (
            <div className="space-y-2">
              <span className="text-sm font-medium">Skill Instructions (Markdown)</span>
              <textarea
                className="w-full h-48 rounded-md border bg-background p-3 font-mono text-sm resize-none focus:outline-none focus:ring-1 focus:ring-ring"
                placeholder={`# ${name || "My Skill"}\n\nInstructions for the agent on how to use this skill.`}
                value={body}
                onChange={(e) => setBody(e.target.value)}
              />
            </div>
          )}

          {step === 3 && (
            <div className="space-y-2">
              <span className="text-sm font-medium">Preview SKILL.md</span>
              <pre className="text-xs bg-muted rounded-md p-3 max-h-64 overflow-auto whitespace-pre-wrap font-mono">
                {generateSkillMd()}
              </pre>
            </div>
          )}
        </div>

        <DialogFooter>
          <div className="flex w-full justify-between">
            <Button variant="outline" onClick={step === 0 ? handleClose : () => setStep(step - 1)}>
              {step === 0 ? "Cancel" : "Back"}
            </Button>
            {step < STEPS.length - 1 ? (
              <Button disabled={!canProceed()} onClick={() => setStep(step + 1)}>
                Next
              </Button>
            ) : (
              <Button disabled={saveMutation.isPending} onClick={handleCreate}>
                {saveMutation.isPending ? "Creating..." : "Create Skill"}
              </Button>
            )}
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
