import { DiffEditor } from "@monaco-editor/react";

interface WorkflowDiffProps {
  original: string;
  modified: string;
  /** Monaco language id; defaults to TOML (workflow/playbook sources). */
  language?: string;
}

export function WorkflowDiff({ original, modified, language = "toml" }: WorkflowDiffProps) {
  return (
    <div className="h-full w-full">
      <DiffEditor
        original={original}
        modified={modified}
        language={language}
        theme="vs-dark"
        options={{
          readOnly: true,
          renderSideBySide: true,
          minimap: { enabled: false },
          fontSize: 13,
          scrollBeyondLastLine: false,
          automaticLayout: true,
        }}
      />
    </div>
  );
}
