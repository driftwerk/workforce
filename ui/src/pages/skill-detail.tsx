import { useState, useCallback, useEffect } from "react";
import { useParams, Link, useNavigate } from "react-router";
import { parse as parseToml } from "smol-toml";
import { ArrowLeft, Save, Loader2, Trash2, Key, FileText } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { TomlEditor } from "@/components/workflows/toml-editor";
import {
  useGlobalSkills,
  useGlobalSkillContent,
  useSaveGlobalSkill,
  useDeleteGlobalSkill,
  useGlobalSkillConfig,
  useSaveGlobalSkillConfig,
} from "@/hooks/use-skills";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { GlobalSkill } from "@/lib/api/skills";

function BackLink() {
  return (
    <Link to="/skills" className="flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
      <ArrowLeft className="h-4 w-4" /> Back to Skills
    </Link>
  );
}

export default function SkillDetailPage() {
  const { name } = useParams<{ name: string }>();
  const { data: skillsData, isLoading, error } = useGlobalSkills();
  const skill = skillsData?.skills.find(
    (s) => s.dir_name === name || s.name === name,
  );

  if (error) {
    return (
      <div className="p-6 space-y-4">
        <BackLink />
        <div className="text-center py-12 text-destructive text-sm">
          Failed to load skills: {error instanceof Error ? error.message : "Unknown error"}
        </div>
      </div>
    );
  }

  if (isLoading || !skill) {
    return (
      <div className="p-6 space-y-4">
        <BackLink />
        <div className="flex items-center justify-center h-64">
          <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
        </div>
      </div>
    );
  }

  const dirName = skill.dir_name ?? skill.name;
  const showConfig = skill.has_config || skill.supports_config === true;
  const showInstructions = skill.editable;
  const defaultTab = showConfig ? "config" : showInstructions ? "instructions" : "overview";

  return (
    <div className="p-6 space-y-4">
      <BackLink />

      <div className="flex items-center gap-3">
        <div>
          <div className="flex items-center gap-2">
            <h1 className="text-lg font-semibold">{skill.name}</h1>
            <Badge variant={skill.type === "builtin" ? "secondary" : "outline"}>
              {skill.type === "builtin" ? "Built-in" : "External"}
            </Badge>
          </div>
          {skill.description && (
            <p className="text-sm text-muted-foreground mt-1">{skill.description}</p>
          )}
        </div>
      </div>

      <Tabs defaultValue={defaultTab}>
        <TabsList>
          <TabsTrigger value="overview">Overview</TabsTrigger>
          {showConfig && <TabsTrigger value="config">Configuration</TabsTrigger>}
          {showInstructions && <TabsTrigger value="instructions">Instructions</TabsTrigger>}
        </TabsList>

        <TabsContent value="overview">
          <OverviewTab skill={skill} />
        </TabsContent>

        {showConfig && (
          <TabsContent value="config">
            <ConfigTab dirName={dirName} />
          </TabsContent>
        )}

        {showInstructions && (
          <TabsContent value="instructions">
            <InstructionsTab dirName={dirName} />
          </TabsContent>
        )}
      </Tabs>
    </div>
  );
}

function OverviewTab({ skill }: { skill: GlobalSkill }) {
  return (
    <div className="mt-4 space-y-6">
      {skill.required_credentials.length > 0 && (
        <div>
          <h3 className="text-sm font-medium mb-2 flex items-center gap-2">
            <Key className="h-4 w-4" /> Required Credentials
          </h3>
          <p className="text-xs text-muted-foreground mb-3">
            These vault secrets must be configured per-agent in the agent's Skills tab.
          </p>
          <div className="space-y-2">
            {skill.required_credentials.map((cred) => (
              <div key={cred.vault_path} className="flex items-center gap-3 rounded-md border px-3 py-2 text-sm">
                <code className="text-xs bg-muted px-1.5 py-0.5 rounded font-mono">{cred.vault_path}</code>
                <span className="text-muted-foreground">{cred.description}</span>
                {cred.required && <Badge variant="destructive" className="text-[10px]">Required</Badge>}
              </div>
            ))}
          </div>
        </div>
      )}

      {skill.required_credentials.length === 0 && !skill.has_config && (
        <div className="text-sm text-muted-foreground py-8 text-center">
          This skill requires no configuration.
        </div>
      )}

      {skill.setup_hint && (
        <div>
          <h3 className="text-sm font-medium mb-2">Setup Instructions</h3>
          <pre className="text-xs bg-muted rounded-md p-3 whitespace-pre-wrap font-mono">{skill.setup_hint}</pre>
        </div>
      )}
    </div>
  );
}

function ConfigTab({ dirName }: { dirName: string }) {
  const { data: configContent, isLoading, error } = useGlobalSkillConfig(dirName);
  const saveMutation = useSaveGlobalSkillConfig();

  const [toml, setToml] = useState("");
  const [dirty, setDirty] = useState(false);
  const [parseError, setParseError] = useState<string | null>(null);
  const [parseErrorLine, setParseErrorLine] = useState<number | null>(null);

  useEffect(() => {
    if (isLoading) return;
    if (configContent && configContent.trim().length > 0) {
      setToml(configContent);
    } else {
      setToml(`# ${dirName} configuration\n`);
    }
    setDirty(false);
    setParseError(null);
    setParseErrorLine(null);
  }, [configContent, isLoading, dirName]);

  const handleChange = useCallback((val: string) => {
    setToml(val);
    setDirty(true);
    try {
      parseToml(val);
      setParseError(null);
      setParseErrorLine(null);
    } catch (e: unknown) {
      if (e && typeof e === "object" && "message" in e) {
        const msg = (e as { message: string }).message;
        setParseError(msg);
        const lineMatch = msg.match(/line (\d+)/i);
        setParseErrorLine(lineMatch ? parseInt(lineMatch[1]!, 10) : null);
      }
    }
  }, []);

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (error) {
    return (
      <div className="mt-4 text-destructive text-sm">
        Failed to load configuration: {error instanceof Error ? error.message : "Unknown error"}
      </div>
    );
  }

  return (
    <div className="mt-4 space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-xs text-muted-foreground">
          Global configuration for <span className="font-mono font-medium">{dirName}</span>
        </p>
        <Button
          size="sm"
          disabled={!dirty || !!parseError || saveMutation.isPending}
          onClick={() => saveMutation.mutate({ name: dirName, content: toml }, { onSuccess: () => setDirty(false) })}
        >
          <Save className="h-3.5 w-3.5 mr-1" />
          {saveMutation.isPending ? "Saving..." : "Save Config"}
        </Button>
      </div>
      <div className="border rounded-lg overflow-hidden" style={{ height: 450 }}>
        <TomlEditor
          value={toml}
          onChange={handleChange}
          parseError={parseError}
          parseErrorLine={parseErrorLine}
        />
      </div>
    </div>
  );
}

function InstructionsTab({ dirName }: { dirName: string }) {
  const navigate = useNavigate();
  const { data: content, isLoading, error } = useGlobalSkillContent(dirName);
  const saveMutation = useSaveGlobalSkill();
  const deleteMutation = useDeleteGlobalSkill();

  const [edited, setEdited] = useState("");
  const [dirty, setDirty] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);

  useEffect(() => {
    if (content) {
      setEdited(content);
      setDirty(false);
    }
  }, [content]);

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (error) {
    return (
      <div className="mt-4 text-destructive text-sm">
        Failed to load instructions: {error instanceof Error ? error.message : "Unknown error"}
      </div>
    );
  }

  return (
    <div className="mt-4 space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-xs text-muted-foreground flex items-center gap-1">
          <FileText className="h-3 w-3" /> SKILL.md
        </p>
        <div className="flex items-center gap-2">
          <Button variant="destructive" size="sm" onClick={() => setDeleteOpen(true)}>
            <Trash2 className="h-3.5 w-3.5 mr-1" />
            Delete
          </Button>
          <Button
            size="sm"
            disabled={!dirty || saveMutation.isPending}
            onClick={() =>
              saveMutation.mutate(
                { name: dirName, content: edited },
                { onSuccess: () => setDirty(false) },
              )
            }
          >
            <Save className="h-3.5 w-3.5 mr-1" />
            {saveMutation.isPending ? "Saving..." : "Save"}
          </Button>
        </div>
      </div>
      <div className="border rounded-lg overflow-hidden" style={{ height: 450 }}>
        <textarea
          className="w-full h-full p-4 font-mono text-sm resize-none bg-background focus:outline-none"
          value={edited}
          onChange={(e) => {
            setEdited(e.target.value);
            setDirty(true);
          }}
        />
      </div>

      <Dialog open={deleteOpen} onOpenChange={setDeleteOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Skill</DialogTitle>
            <DialogDescription>
              Are you sure you want to delete <strong>{dirName}</strong>? This cannot be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteOpen(false)}>Cancel</Button>
            <Button
              variant="destructive"
              onClick={() => {
                deleteMutation.mutate(dirName, {
                  onSuccess: () => {
                    setDeleteOpen(false);
                    navigate("/skills");
                  },
                });
              }}
            >
              Delete
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
