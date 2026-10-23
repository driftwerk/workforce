import { useState } from "react";
import { Link } from "react-router";
import { Puzzle, Plus, Loader2, CheckCircle2, Settings, FileText } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useGlobalSkills } from "@/hooks/use-skills";
import { SkillCreationWizard } from "@/components/skills/skill-creation-wizard";
import { SkillCatalog } from "@/components/skills/skill-catalog";
import { OwnedSkills } from "@/components/skills/owned-skills";

export default function SkillsPage() {
  const { data, isLoading, error } = useGlobalSkills();
  const [wizardOpen, setWizardOpen] = useState(false);

  const skills = data?.skills ?? [];

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="rounded-full bg-muted p-2">
            <Puzzle className="h-5 w-5 text-muted-foreground" />
          </div>
          <div>
            <h1 className="text-lg font-semibold">Skills</h1>
            <p className="text-sm text-muted-foreground">
              Manage skill definitions and configuration.
            </p>
          </div>
        </div>
        <Button size="sm" onClick={() => setWizardOpen(true)}>
          <Plus className="h-4 w-4 mr-1" />
          New Skill
        </Button>
      </div>

      {error ? (
        <div className="text-center py-12 text-destructive text-sm">
          Failed to load skills: {error instanceof Error ? error.message : "Unknown error"}
        </div>
      ) : isLoading ? (
        <div className="flex items-center justify-center h-64">
          <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
        </div>
      ) : skills.length === 0 ? (
        <div className="text-center py-12 text-muted-foreground">
          No skills found.
        </div>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {skills.map((skill) => (
            <Link
              key={skill.dir_name}
              to={`/skills/${skill.dir_name}`}
              className="block rounded-lg border bg-card p-4 hover:bg-accent/50 transition-colors"
            >
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="font-medium text-sm truncate">{skill.name}</span>
                    <Badge variant={skill.type === "builtin" ? "secondary" : "outline"} className="text-[10px] shrink-0">
                      {skill.type === "builtin" ? "Built-in" : "External"}
                    </Badge>
                  </div>
                  {skill.description && (
                    <p className="text-xs text-muted-foreground mt-1 line-clamp-2">
                      {skill.description}
                    </p>
                  )}
                </div>
              </div>
              <div className="flex items-center gap-3 mt-3 text-xs text-muted-foreground">
                {skill.has_config && (
                  <span className="flex items-center gap-1">
                    <Settings className="h-3 w-3" />
                    Config
                  </span>
                )}
                {skill.required_credentials.length > 0 && (
                  <span className="flex items-center gap-1">
                    <CheckCircle2 className="h-3 w-3" />
                    {skill.required_credentials.length} credential{skill.required_credentials.length !== 1 ? "s" : ""}
                  </span>
                )}
                {skill.editable && (
                  <span className="flex items-center gap-1">
                    <FileText className="h-3 w-3" />
                    Editable
                  </span>
                )}
              </div>
            </Link>
          ))}
        </div>
      )}

      <OwnedSkills kind="external-skills" />

      <OwnedSkills kind="wasm-skills" />

      <SkillCatalog />

      <SkillCreationWizard open={wizardOpen} onClose={() => setWizardOpen(false)} />
    </div>
  );
}
