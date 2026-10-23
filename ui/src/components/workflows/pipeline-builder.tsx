import { useCallback, useEffect, useMemo } from "react";
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
import { StepNode } from "./nodes/step-node";
import { PauseNode } from "./nodes/pause-node";
import { ExitNode } from "./nodes/exit-node";
import { BranchEdge } from "./edges/branch-edge";
import { GotoEdge } from "./edges/goto-edge";
import { tomlToGraph } from "@/lib/workflows/toml-parser";
import { computeLayout } from "@/lib/workflows/layout";
import { Button } from "@/components/ui/button";
import { LayoutGrid } from "lucide-react";
import type { WorkflowDefinition } from "@/lib/api/workflows";

const nodeTypes: NodeTypes = {
  normalStep: StepNode,
  branchStep: StepNode,
  pauseStep: PauseNode,
  exitStep: ExitNode,
};

const edgeTypes: EdgeTypes = {
  branchEdge: BranchEdge,
  decisionEdge: BranchEdge,
  gotoEdge: GotoEdge,
};

interface PipelineBuilderProps {
  definition: WorkflowDefinition | null;
}

export function PipelineBuilder({ definition }: PipelineBuilderProps) {
  const [nodes, setNodes, onNodesChange] = useNodesState<RFNode>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<RFEdge>([]);

  // Convert definition to graph when it changes
  useEffect(() => {
    if (!definition) {
      setNodes([]);
      setEdges([]);
      return;
    }
    const { nodes: rawNodes, edges: newEdges } = tomlToGraph(definition);
    const layoutNodes = computeLayout(rawNodes, newEdges);
    setNodes(layoutNodes as RFNode[]);
    setEdges(newEdges as RFEdge[]);
  }, [definition, setNodes, setEdges]);

  const handleAutoLayout = useCallback(() => {
    setNodes((prev) => {
      const laid = computeLayout(prev, edges);
      return laid as RFNode[];
    });
  }, [edges, setNodes]);

  const svgDefs = useMemo(
    () => (
      <svg style={{ position: "absolute", width: 0, height: 0 }}>
        <defs>
          <marker id="branch-arrow" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="fill-primary" />
          </marker>
          <marker id="goto-arrow" viewBox="0 0 10 10" refX="10" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" className="fill-muted-foreground/50" />
          </marker>
        </defs>
      </svg>
    ),
    [],
  );

  if (!definition) {
    return (
      <div className="flex items-center justify-center h-full text-muted-foreground">
        No valid workflow to display
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
