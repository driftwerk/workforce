import { useCallback, useRef } from "react";
import Editor, { type OnMount } from "@monaco-editor/react";
import type { editor } from "monaco-editor";
import { EDITOR_OPTIONS } from "@/lib/monaco-setup";

interface TomlEditorProps {
  value: string;
  onChange: (value: string) => void;
  parseError: string | null;
  parseErrorLine: number | null;
  readOnly?: boolean;
}

export function TomlEditor({ value, onChange, parseError, parseErrorLine, readOnly = false }: TomlEditorProps) {
  const editorRef = useRef<editor.IStandaloneCodeEditor | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const handleMount: OnMount = useCallback((editor, monaco) => {
    editorRef.current = editor;

    // Register TOML as a simple language if not already registered
    const langs = monaco.languages.getLanguages();
    if (!langs.some((l: { id: string }) => l.id === "toml")) {
      monaco.languages.register({ id: "toml" });
      monaco.languages.setMonarchTokensProvider("toml", {
        tokenizer: {
          root: [
            [/#.*$/, "comment"],
            [/\[\[?[^\]]*\]\]?/, "keyword"],
            [/"[^"]*"/, "string"],
            [/'[^']*'/, "string"],
            [/"""[\s\S]*?"""/, "string"],
            [/'''[\s\S]*?'''/, "string"],
            [/\b(true|false)\b/, "keyword"],
            [/\b\d+(\.\d+)?\b/, "number"],
            [/[a-zA-Z_][a-zA-Z0-9_-]*\s*=/, "variable"],
          ],
        },
      });
    }

    // Set error markers if we have a parse error
    if (parseError && parseErrorLine) {
      const model = editor.getModel();
      if (model) {
        monaco.editor.setModelMarkers(model, "toml-parser", [
          {
            startLineNumber: parseErrorLine,
            startColumn: 1,
            endLineNumber: parseErrorLine,
            endColumn: model.getLineMaxColumn(parseErrorLine),
            message: parseError,
            severity: monaco.MarkerSeverity.Error,
          },
        ]);
      }
    }
  }, [parseError, parseErrorLine]);

  const handleChange = useCallback(
    (val: string | undefined) => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
      debounceRef.current = setTimeout(() => {
        onChange(val ?? "");
      }, 300);
    },
    [onChange],
  );

  // Update markers when parseError changes
  const updateMarkers = useCallback(() => {
    const editor = editorRef.current;
    if (!editor) return;
    const model = editor.getModel();
    if (!model) return;

    // Access monaco through the editor
    const monacoInstance = (window as unknown as Record<string, unknown>).monaco as typeof import("monaco-editor") | undefined;
    if (!monacoInstance) return;

    if (parseError && parseErrorLine) {
      monacoInstance.editor.setModelMarkers(model, "toml-parser", [
        {
          startLineNumber: parseErrorLine,
          startColumn: 1,
          endLineNumber: parseErrorLine,
          endColumn: model.getLineMaxColumn(parseErrorLine),
          message: parseError,
          severity: monacoInstance.MarkerSeverity.Error,
        },
      ]);
    } else {
      monacoInstance.editor.setModelMarkers(model, "toml-parser", []);
    }
  }, [parseError, parseErrorLine]);

  // Effect handled by onMount + marker updates
  if (editorRef.current) {
    updateMarkers();
  }

  return (
    <div className="h-full w-full flex flex-col">
      {parseError && (
        <div className="px-3 py-1.5 bg-destructive/10 text-destructive text-xs border-b border-border">
          Parse error{parseErrorLine ? ` (line ${parseErrorLine})` : ""}: {parseError}
        </div>
      )}
      <div className="flex-1 min-h-0">
        <Editor
          language="toml"
          theme="vs-dark"
          value={value}
          onChange={handleChange}
          onMount={handleMount}
          options={{ ...EDITOR_OPTIONS, readOnly }}
        />
      </div>
    </div>
  );
}
