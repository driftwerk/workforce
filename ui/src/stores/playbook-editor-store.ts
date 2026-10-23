import { create } from "zustand";
import type { PlaybookDefinition } from "@/lib/api/playbooks";
import { parsePlaybookToml } from "@/lib/playbooks/toml-parser";

export type ViewMode = "visual" | "code" | "split";

interface PlaybookEditorState {
  tomlSource: string;
  parsedDef: PlaybookDefinition | null;
  parseError: string | null;
  parseErrorLine: number | null;
  viewMode: ViewMode;
  isDirty: boolean;

  setToml: (src: string) => void;
  setViewMode: (mode: ViewMode) => void;
  markSaved: () => void;
  reset: () => void;
}

const INITIAL = {
  tomlSource: "",
  parsedDef: null,
  parseError: null,
  parseErrorLine: null,
  viewMode: "split" as ViewMode,
  isDirty: false,
};

export const usePlaybookEditorStore = create<PlaybookEditorState>((set) => ({
  ...INITIAL,

  setToml: (src: string) => {
    const { def, error, errorLine } = parsePlaybookToml(src);
    set({
      tomlSource: src,
      parsedDef: def,
      parseError: error,
      parseErrorLine: errorLine,
      isDirty: true,
    });
  },

  setViewMode: (mode: ViewMode) => set({ viewMode: mode }),
  markSaved: () => set({ isDirty: false }),
  reset: () => set({ ...INITIAL }),
}));
