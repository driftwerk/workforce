import { useRef, useState } from "react";
import { ArrowLeftRight, Download, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { EntityMultiCombobox } from "@/components/permissions/entity-picker";
import { useAgentTransfer } from "@/hooks/use-agent-transfer";
import type { ImportOutcome, MergePlan } from "@/lib/api/agent-transfer";
import { downloadBlob } from "@/lib/download";

export default function AgentTransferPage() {
  const { exportBundle, uploadBundle, applyImport } = useAgentTransfer();
  const [selected, setSelected] = useState<string[]>([]);
  const [artifactId, setArtifactId] = useState<string | null>(null);
  const [plan, setPlan] = useState<MergePlan | null>(null);
  const [outcome, setOutcome] = useState<ImportOutcome | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);

  const handleExport = () =>
    exportBundle.mutate(selected, {
      onSuccess: (blob) => downloadBlob(blob, "workforce-export.zip"),
    });

  const handleFile = (event: React.ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (!file) return;
    setOutcome(null);
    setPlan(null);
    setArtifactId(null);
    uploadBundle.mutate(file, {
      onSuccess: ({ artifact_id, plan: merge }) => {
        setArtifactId(artifact_id);
        setPlan(merge);
      },
    });
  };

  const handleApply = () => {
    if (!artifactId) return;
    applyImport.mutate(
      {
        artifactId,
        options: { renames: {}, identity_overrides: {}, confirm: true },
      },
      {
        onSuccess: (result) => {
          setOutcome(result);
          setPlan(null);
          setArtifactId(null);
        },
      },
    );
  };

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-2">
        <ArrowLeftRight className="h-5 w-5" />
        <h1 className="text-xl font-semibold">Agent Transfer</h1>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>Export</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <p className="text-sm text-muted-foreground">
            Bundle the selected agents — their config, conversation transcripts
            and memories — into a portable <code>.zip</code>. Secrets and the
            knowledge base are not included.
          </p>
          <EntityMultiCombobox
            kind="agent"
            values={selected}
            onChange={setSelected}
          />
          <Button
            onClick={handleExport}
            disabled={selected.length === 0 || exportBundle.isPending}
          >
            <Download className="h-4 w-4" />
            {exportBundle.isPending ? "Exporting…" : "Export bundle"}
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Import</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          <p className="text-sm text-muted-foreground">
            Upload a bundle to preview the merge plan, then apply it. Agents that
            collide are renamed; users are reconciled by email.
          </p>
          <input
            ref={fileInput}
            type="file"
            accept=".zip"
            className="hidden"
            onChange={handleFile}
          />
          <Button
            variant="outline"
            onClick={() => fileInput.current?.click()}
            disabled={uploadBundle.isPending}
          >
            <Upload className="h-4 w-4" />
            {uploadBundle.isPending ? "Reading…" : "Choose bundle…"}
          </Button>

          {plan && (
            <div className="space-y-3 rounded-md border p-4">
              <p className="text-sm">
                Source install:{" "}
                <span className="font-mono">{plan.source_install_id}</span>
              </p>
              {plan.agents.map((agent) => (
                <div key={agent.source_id} className="space-y-1 text-sm">
                  <div className="flex items-center gap-2">
                    <span className="font-medium">{agent.source_id}</span>
                    {agent.source_id !== agent.target_id && (
                      <span className="text-muted-foreground">
                        → {agent.target_id}
                      </span>
                    )}
                    {agent.collides && (
                      <Badge variant="destructive">collision</Badge>
                    )}
                    <span className="text-muted-foreground">
                      {agent.sessions} session(s), {agent.memories} memory(s)
                    </span>
                  </div>
                  {agent.expected_credentials.map((credential) => (
                    <p key={credential} className="text-xs text-muted-foreground">
                      provision: {credential}
                    </p>
                  ))}
                </div>
              ))}
              <p className="text-sm text-muted-foreground">
                Identities:{" "}
                {plan.identities.filter((i) => i.resolution.kind === "matched")
                  .length}{" "}
                matched,{" "}
                {plan.identities.filter((i) => i.resolution.kind === "create")
                  .length}{" "}
                to create
              </p>
              <Button onClick={handleApply} disabled={applyImport.isPending}>
                {applyImport.isPending ? "Applying…" : "Apply import"}
              </Button>
            </div>
          )}

          {outcome && (
            <div className="space-y-1 rounded-md border p-4 text-sm">
              <p>
                Imported {outcome.agents_imported.length} agent(s),{" "}
                {outcome.sessions_imported} session(s), {outcome.memories_imported}{" "}
                memory(s) ({outcome.memories_skipped} skipped),{" "}
                {outcome.identities_created} identity(s) created,{" "}
                {outcome.identities_matched} matched.
              </p>
              {outcome.notes.map((note) => (
                <p key={note} className="text-xs text-muted-foreground">
                  {note}
                </p>
              ))}
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
