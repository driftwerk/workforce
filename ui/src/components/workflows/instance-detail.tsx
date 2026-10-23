import { useState, useEffect, useMemo } from "react";
import {
  ReactFlow,
  ReactFlowProvider,
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
import { InstanceStatusBadge } from "@/components/workflows/instance-status-badge";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Skeleton } from "@/components/ui/skeleton";
import {
  useWorkflowInstance,
  useInstanceSteps,
  useInstanceCost,
  useWorkflowSource,
} from "@/hooks/use-workflows";
import { StepTimeline } from "./step-timeline";
import { StepDetailPanel } from "./step-detail-panel";
import { HumanInteractionPanel } from "./human-interaction-panel";
import { InstanceActionsToolbar } from "./instance-actions-toolbar";
import { InstanceCostSummary } from "./instance-cost-summary";
import { InstanceStepNode } from "./nodes/instance-step-node";
import { BranchEdge } from "./edges/branch-edge";
import { GotoEdge } from "./edges/goto-edge";
import { formatDateTime, formatRelativeTime } from "@/lib/utils";
import type { InstanceStep, TimelineStep, WorkflowDefinition } from "@/lib/api/workflows";
import { splitWorkflowKey } from "@/lib/api/workflows";
import { parseWorkflowToml, tomlToGraph } from "@/lib/workflows/toml-parser";
import { computeLayout } from "@/lib/workflows/layout";
import { ArrowLeft } from "lucide-react";

// --- Instance pipeline node/edge types ---

const instanceNodeTypes: NodeTypes = {
  normalStep: InstanceStepNode,
  branchStep: InstanceStepNode,
  pauseStep: InstanceStepNode,
  exitStep: InstanceStepNode,
};

const instanceEdgeTypes: EdgeTypes = {
  branchEdge: BranchEdge,
  decisionEdge: BranchEdge,
  gotoEdge: GotoEdge,
};

// --- Instance Pipeline View (internal) ---

interface InstancePipelineViewProps {
  definition: WorkflowDefinition | null;
  stepStatusMap: Map<string, string>;
  currentStepId: string | null;
  onNodeClick: (stepId: string) => void;
}

function InstancePipelineView({ definition, stepStatusMap, currentStepId, onNodeClick }: InstancePipelineViewProps) {
  const [nodes, setNodes, onNodesChange] = useNodesState<RFNode>([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState<RFEdge>([]);

  useEffect(() => {
    if (!definition) {
      setNodes([]);
      setEdges([]);
      return;
    }
    const { nodes: rawNodes, edges: newEdges } = tomlToGraph(definition);
    const augmentedNodes = rawNodes.map((node) => ({
      ...node,
      data: {
        ...node.data,
        instanceStatus: stepStatusMap.get(node.id) ?? "pending",
        isCurrent: node.id === currentStepId,
      },
    }));
    const layoutNodes = computeLayout(augmentedNodes, newEdges);
    setNodes(layoutNodes as RFNode[]);
    setEdges(newEdges as RFEdge[]);
  }, [definition, stepStatusMap, currentStepId, setNodes, setEdges]);

  if (!definition) {
    return (
      <div className="flex items-center justify-center h-full text-muted-foreground text-sm">
        No pipeline definition available
      </div>
    );
  }

  return (
    <div className="h-full w-full relative">
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
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        nodeTypes={instanceNodeTypes}
        edgeTypes={instanceEdgeTypes}
        onNodeClick={(_event, node) => onNodeClick(node.id)}
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
    </div>
  );
}

// --- Main Instance Detail Component ---

interface InstanceDetailProps {
  instanceId: string;
  onBack: () => void;
}

export function InstanceDetail({ instanceId, onBack }: InstanceDetailProps) {
  const { data: instance, isLoading: loadingInstance } = useWorkflowInstance(instanceId);
  const { data: stepsData, isLoading: loadingSteps } = useInstanceSteps(instanceId);
  const { data: costData } = useInstanceCost(instanceId);

  // An instance's workflow_id is the internal {owner}/{id} addressing key;
  // split it into the bare id + owner the source endpoint expects.
  const { id: bareWorkflowId, owner: workflowOwner } = instance?.workflow_id
    ? splitWorkflowKey(instance.workflow_id)
    : { id: undefined, owner: null };
  const { data: source } = useWorkflowSource(bareWorkflowId, workflowOwner);

  const [selectedStepId, setSelectedStepId] = useState<string | null>(null);

  // Parse workflow definition from TOML source
  const parsedDef = useMemo<WorkflowDefinition | null>(() => {
    if (!source || typeof source !== "string") return null;
    const result = parseWorkflowToml(source);
    return result.def;
  }, [source]);

  const steps = useMemo<InstanceStep[]>(() => stepsData?.steps ?? [], [stepsData]);

  // Every recorded step, named from the definition, then the step the
  // instance is on now when nothing has been recorded for it yet.
  const timeline = useMemo<TimelineStep[]>(() => {
    const nameOf = (id: string) => parsedDef?.step.find((s) => s.id === id)?.name ?? id;
    const recorded: TimelineStep[] = steps.map((record) => ({
      step_id: record.step_id,
      name: nameOf(record.step_id),
      status: record.status,
      record,
    }));
    const current = instance?.current_step;
    const live = { running: "running", paused: "paused", failed: "failed" }[instance?.status ?? ""];
    if (current && live && !recorded.some((s) => s.step_id === current && s.status === "failed")) {
      recorded.push({ step_id: current, name: nameOf(current), status: live, record: null });
    }
    return recorded;
  }, [steps, parsedDef, instance?.current_step, instance?.status]);

  const selectedStep = timeline.find((s) => s.step_id === selectedStepId) ?? null;

  // Build the step status map for coloring nodes
  const stepStatusMap = useMemo(() => {
    const map = new Map<string, string>();
    for (const step of timeline) {
      map.set(step.step_id, step.status);
    }
    return map;
  }, [timeline]);

  // Find branches for the current paused step from the definition
  const currentStepBranches = useMemo(() => {
    if (!parsedDef || !instance?.current_step) return [];
    const stepDef = parsedDef.step.find((s) => s.id === instance.current_step);
    return stepDef?.branches ?? [];
  }, [parsedDef, instance?.current_step]);

  // Find the current step in the timeline
  const currentStepData = useMemo(() => {
    if (!instance?.current_step) return null;
    return timeline.find((s) => s.step_id === instance.current_step) ?? null;
  }, [instance?.current_step, timeline]);

  // Auto-select current step
  useEffect(() => {
    if (instance?.current_step && !selectedStepId) {
      setSelectedStepId(instance.current_step);
    }
  }, [instance?.current_step, selectedStepId]);

  // Build an augmented definition that applies step status colors via node data
  const augmentedDef = useMemo<WorkflowDefinition | null>(() => {
    if (!parsedDef) return null;
    return {
      ...parsedDef,
      step: parsedDef.step.map((s) => ({
        ...s,
      })),
    };
  }, [parsedDef]);

  if (loadingInstance) {
    return (
      <div className="p-4 space-y-4">
        <Skeleton className="h-8 w-64" />
        <Skeleton className="h-64 w-full" />
      </div>
    );
  }

  if (!instance) {
    return (
      <div className="p-4">
        <Button variant="ghost" size="sm" onClick={onBack}>
          <ArrowLeft className="h-4 w-4 mr-1" />
          Back
        </Button>
        <div className="flex items-center justify-center py-12 text-sm text-muted-foreground">
          Instance not found
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <div className="flex items-center justify-between gap-3 px-4 py-2.5 border-b bg-card shrink-0">
        <div className="flex items-center gap-3 min-w-0">
          <Button variant="ghost" size="icon" className="h-8 w-8 shrink-0" onClick={onBack}>
            <ArrowLeft className="h-4 w-4" />
          </Button>
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h2 className="text-sm font-semibold truncate">Instance</h2>
              <InstanceStatusBadge status={instance.status} className="text-[10px]" />
            </div>
            <span className="text-xs text-muted-foreground font-mono block truncate">
              {instance.instance_id}
            </span>
          </div>
          <span className="text-xs text-muted-foreground shrink-0" title={formatRelativeTime(instance.created_at)}>
            Started {formatDateTime(instance.created_at)}
          </span>
        </div>
        <InstanceActionsToolbar
          instance={instance}
          cost={costData}
          steps={steps}
          currentStepId={instance.current_step}
        />
      </div>

      {/* A failed run is otherwise a bare status badge with no stated cause */}
      {instance.status === "failed" && instance.failure_reason && (
        <div className="px-4 pt-3">
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2">
            <div className="text-xs font-semibold text-destructive">Failure reason</div>
            <p className="mt-0.5 text-xs text-destructive/90 whitespace-pre-wrap break-words">
              {instance.failure_reason}
            </p>
          </div>
        </div>
      )}

      {/* Human interaction panel if paused */}
      {instance.status === "paused" && (
        <div className="px-4 pt-3">
          <HumanInteractionPanel
            instance={instance}
            currentStep={currentStepData}
            branches={currentStepBranches}
          />
        </div>
      )}

      {/* Main content: tabs for pipeline/timeline/cost */}
      <Tabs defaultValue="pipeline" className="flex flex-col flex-1 min-h-0">
        <TabsList className="h-auto bg-transparent rounded-none border-b px-4 justify-start gap-0 p-0">
          <TabsTrigger
            value="pipeline"
            className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
          >
            Pipeline
          </TabsTrigger>
          <TabsTrigger
            value="timeline"
            className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
          >
            Timeline
          </TabsTrigger>
          <TabsTrigger
            value="cost"
            className="h-9 text-xs px-4 rounded-none border-b-2 border-transparent data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:shadow-none"
          >
            Cost
          </TabsTrigger>
        </TabsList>

        <TabsContent value="pipeline" className="flex-1 min-h-0 m-0">
          <div className="flex h-full">
            <div className="flex-1 min-w-0 h-full">
              <ReactFlowProvider>
                <InstancePipelineView
                  definition={augmentedDef}
                  stepStatusMap={stepStatusMap}
                  currentStepId={instance.current_step}
                  onNodeClick={setSelectedStepId}
                />
              </ReactFlowProvider>
            </div>
            {selectedStep && (
              <div className="w-80 border-l overflow-y-auto p-3 shrink-0">
                <StepDetailPanel step={selectedStep} />
              </div>
            )}
          </div>
        </TabsContent>

        <TabsContent value="timeline" className="flex-1 min-h-0 m-0">
          <div className="flex h-full">
            <div className="flex-1 min-w-0 overflow-y-auto p-4">
              {loadingSteps ? (
                <div className="space-y-3">
                  {Array.from({ length: 4 }).map((_, i) => (
                    <Skeleton key={i} className="h-14 w-full" />
                  ))}
                </div>
              ) : (
                <StepTimeline
                  steps={timeline}
                  selectedStepId={selectedStepId}
                  onSelectStep={setSelectedStepId}
                />
              )}
            </div>
            {selectedStep && (
              <div className="w-80 border-l overflow-y-auto p-3 shrink-0">
                <StepDetailPanel step={selectedStep} />
              </div>
            )}
          </div>
        </TabsContent>

        <TabsContent value="cost" className="flex-1 min-h-0 m-0 overflow-y-auto p-4">
          {costData ? (
            <InstanceCostSummary cost={costData} />
          ) : (
            <div className="flex items-center justify-center py-12 text-sm text-muted-foreground">
              No cost data available
            </div>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}
