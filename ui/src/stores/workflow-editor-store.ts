import { create } from "zustand";
import type { WorkflowDefinition } from "@/lib/api/workflows";
import { parseWorkflowToml } from "@/lib/workflows/toml-parser";
import { workflowDefToToml } from "@/lib/workflows/graph-to-toml";

export type ViewMode = "visual" | "code" | "split";

interface WorkflowEditorState {
  tomlSource: string;
  parsedDef: WorkflowDefinition | null;
  parseError: string | null;
  parseErrorLine: number | null;
  viewMode: ViewMode;
  isDirty: boolean;
  activeStepId: string | null;

  setToml: (src: string) => void;
  setFromDef: (def: WorkflowDefinition) => void;
  setViewMode: (mode: ViewMode) => void;
  setActiveStep: (id: string | null) => void;
  markSaved: () => void;
  reset: () => void;
}

export const useWorkflowEditorStore = create<WorkflowEditorState>((set) => ({
  tomlSource: "",
  parsedDef: null,
  parseError: null,
  parseErrorLine: null,
  viewMode: "split",
  isDirty: false,
  activeStepId: null,

  setToml: (src: string) => {
    const { def, error, errorLine } = parseWorkflowToml(src);
    set({
      tomlSource: src,
      parsedDef: def,
      parseError: error,
      parseErrorLine: errorLine,
      isDirty: true,
    });
  },

  setFromDef: (def: WorkflowDefinition) => {
    const toml = workflowDefToToml(def);
    set({
      tomlSource: toml,
      parsedDef: def,
      parseError: null,
      parseErrorLine: null,
      isDirty: true,
    });
  },

  setViewMode: (mode: ViewMode) => set({ viewMode: mode }),
  setActiveStep: (id: string | null) => set({ activeStepId: id }),
  markSaved: () => set({ isDirty: false }),
  reset: () =>
    set({
      tomlSource: "",
      parsedDef: null,
      parseError: null,
      parseErrorLine: null,
      viewMode: "split",
      isDirty: false,
      activeStepId: null,
    }),
}));
