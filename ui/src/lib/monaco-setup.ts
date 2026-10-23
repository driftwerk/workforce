// Self-host Monaco so the CSP can stay strict (`script-src 'self'`,
// no jsdelivr CDN). @monaco-editor/react's default loader fetches
// `vs/loader.js` from cdn.jsdelivr.net; this module wires it to the
// ESM build bundled into our app instead, and points Monaco's worker
// runtime at Vite-emitted same-origin worker chunks.
//
// Audit follow-up to UI-F2 (2026-05-15).

import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor";

import editorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import jsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";
import cssWorker from "monaco-editor/esm/vs/language/css/css.worker?worker";
import htmlWorker from "monaco-editor/esm/vs/language/html/html.worker?worker";
import tsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";

self.MonacoEnvironment = {
  getWorker(_workerId: string, label: string): Worker {
    switch (label) {
      case "json":
        return new jsonWorker();
      case "css":
      case "scss":
      case "less":
        return new cssWorker();
      case "html":
      case "handlebars":
      case "razor":
        return new htmlWorker();
      case "typescript":
      case "javascript":
        return new tsWorker();
      default:
        return new editorWorker();
    }
  },
};

loader.config({ monaco });

/// Shared options for our editable Monaco editors (TOML, markdown, …).
///
/// These editors hold hand-written prose and config — there is no language
/// server, so every Monaco "suggestion" is just word-from-document
/// autocomplete. Left on (the default), it pops a widget on each keystroke,
/// jumps the cursor, and — worst — accepts a suggestion when you type a commit
/// character such as the space before a closing quote, swallowing the space.
/// Turn the whole suggestion machinery off so typing is just typing.
export const EDITOR_OPTIONS: monaco.editor.IStandaloneEditorConstructionOptions = {
  minimap: { enabled: false },
  fontSize: 13,
  lineNumbers: "on",
  wordWrap: "on",
  scrollBeyondLastLine: false,
  padding: { top: 8 },
  automaticLayout: true,
  quickSuggestions: false,
  suggestOnTriggerCharacters: false,
  wordBasedSuggestions: "off",
  acceptSuggestionOnEnter: "off",
  acceptSuggestionOnCommitCharacter: false,
  tabCompletion: "off",
  parameterHints: { enabled: false },
};
