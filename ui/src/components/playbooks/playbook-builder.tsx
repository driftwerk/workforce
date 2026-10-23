import { useCallback, useEffect, useMemo, useRef } from "react";
import {
  ReactFlow,
  MiniMap,
  Controls,
  Background,
  BackgroundVariant,
  useNodesState,
  useEdgesState,
  type Node as RFNode,
  type Edge as RFEdge,
  type NodeTypes,
  type EdgeTypes,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { LayoutGrid } from "lucide-react";
import { Button } from "@/components/ui/button";
import { PlaybookStepNode } from "./nodes/playbook-step-node";
import { TerminalNode } from "./nodes/terminal-node";
import { ButtonEdge } from "./edges/button-edge";
import { playbookTomlToGraph } from "@/lib/playbooks/toml-parser";
import { computeLayout } from "@/lib/workflows/layout";
import type { PlaybookDefinition } from "@/lib/api/playbooks";

const nodeTypes: NodeTypes = {
  playbookStep: PlaybookStepNode,
  playbookTerminal: TerminalNode,
};

const edgeTypes: EdgeTypes = {
  playbookButton: ButtonEdge,
};

interface PlaybookBuilderProps {
  definition: PlaybookDefinition | null;
  /** Currently-selected step id (highlighted; drives the button editor panel). */
  activeStepId?: string | null;
  /** Fired when a step node is clicked, or null when the pane is clicked. */
  onSelectStep?: (id: string | null) => void;
}

export function PlaybookBuilder({ definition, activeStepId, onSelectStep }: PlaybookBuilderProps) {
  const [nodes, setNodes, onNodesChange] = useNodesState<RFNode>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<RFEdge>([]);
  // Read selection in the layout effect without making it a dependency — a
  // selection change must not trigger a relayout (which would undo manual drags).
  const activeRef = useRef(activeStepId);
  activeRef.current = activeStepId;

  useEffect(() => {
    if (!definition) {
      setNodes([]);
      setEdges([]);
      return;
    }
    const { nodes: rawNodes, edges: newEdges } = playbookTomlToGraph(definition);
    const laid = computeLayout(rawNodes, newEdges).map((n) => ({
      ...n,
      selected: n.id === activeRef.current,
    }));
    setNodes(laid as RFNode[]);
    setEdges(newEdges as RFEdge[]);
  }, [definition, setNodes, setEdges]);

  // Reflect selection changes in place (no relayout).
  useEffect(() => {
    setNodes((prev) => prev.map((n) => ({ ...n, selected: n.id === activeStepId })));
  }, [activeStepId, setNodes]);

  const handleAutoLayout = useCallback(() => {
    setNodes((prev) => computeLayout(prev, edges) as RFNode[]);
  }, [edges, setNodes]);

  const svgDefs = useMemo(
    () => (
      <svg style={{ position: "absolute", width: 0, height: 0 }}>
        <defs>
          <marker id="pb-arrow" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="fill-muted-foreground" />
          </marker>
          <marker id="pb-arrow-error" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="fill-red-500" />
          </marker>
        </defs>
      </svg>
    ),
    [],
  );

  if (!definition) {
    return (
      <div className="flex items-center justify-center h-full text-muted-foreground">
        No valid playbook to display
      </div>
    );
  }

  return (
    <div className="h-full w-full relative">
      {svgDefs}
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onNodeClick={(_, node) =>
          onSelectStep?.(node.type === "playbookStep" ? node.id : null)
        }
        onPaneClick={() => onSelectStep?.(null)}
        nodeTypes={nodeTypes}
        edgeTypes={edgeTypes}
        fitView
        fitViewOptions={{ padding: 0.2 }}
        proOptions={{ hideAttribution: true }}
        className="bg-background"
      >
        <Background variant={BackgroundVariant.Dots} gap={16} size={1} className="!bg-background" />
        <Controls className="!bg-card !border-border !shadow-sm [&>button]:!bg-card [&>button]:!border-border [&>button]:!text-foreground" />
        <MiniMap
          className="!bg-card !border-border"
          nodeColor="oklch(0.488 0.243 264.376)"
          maskColor="oklch(0.145 0 0 / 0.7)"
        />
      </ReactFlow>
      <div className="absolute top-3 right-3 z-10">
        <Button variant="outline" size="sm" onClick={handleAutoLayout}>
          <LayoutGrid className="h-3.5 w-3.5 mr-1.5" />
          Auto Layout
        </Button>
      </div>
    </div>
  );
}
