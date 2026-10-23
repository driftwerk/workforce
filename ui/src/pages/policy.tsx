import { useState } from "react";
import {
  usePolicyRules,
  usePolicyProposals,
  useCreateRule,
  useDeleteRule,
  useApproveProposal,
  useRejectProposal,
} from "@/hooks/use-policy";
import type { PolicyRule } from "@/lib/api/policy";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  Plus,
  Trash2,
  Shield,
  Lock,
  Check,
  X,
  Loader2,
  AlertCircle,
} from "lucide-react";

function CreateRuleDialog() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [scope, setScope] = useState("global");
  const [tool, setTool] = useState("*");
  const [action, setAction] = useState("allow");
  const [error, setError] = useState("");
  const create = useCreateRule();

  const handleCreate = () => {
    setError("");
    if (!name.trim()) return;
    create.mutate(
      { name, scope, tool, action },
      {
        onSuccess: () => {
          setOpen(false);
          setName("");
        },
        onError: (e) => setError(e.message),
      },
    );
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm">
          <Plus className="mr-1.5 h-4 w-4" />
          Add Rule
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create Policy Rule</DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <div>
            <label htmlFor="rule-name" className="text-sm font-medium">
              Name
            </label>
            <Input
              id="rule-name"
              placeholder="e.g. allow-code-tools"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div>
            <label htmlFor="rule-scope" className="text-sm font-medium">
              Scope
            </label>
            <select
              id="rule-scope"
              className="w-full rounded-md border px-3 py-2 text-sm bg-background"
              value={scope}
              onChange={(e) => setScope(e.target.value)}
            >
              <option value="global">Global</option>
              <option value="agent">Agent</option>
              <option value="tool">Tool</option>
            </select>
          </div>
          <div>
            <label htmlFor="rule-tool" className="text-sm font-medium">
              Tool pattern
            </label>
            <Input
              id="rule-tool"
              placeholder="* (all tools) or specific tool name"
              value={tool}
              onChange={(e) => setTool(e.target.value)}
            />
          </div>
          <div>
            <label htmlFor="rule-action" className="text-sm font-medium">
              Action
            </label>
            <select
              id="rule-action"
              className="w-full rounded-md border px-3 py-2 text-sm bg-background"
              value={action}
              onChange={(e) => setAction(e.target.value)}
            >
              <option value="allow">Allow</option>
              <option value="deny">Deny</option>
              <option value="ask">Ask</option>
            </select>
          </div>
          {error && (
            <p className="text-sm text-destructive" role="alert">
              {error}
            </p>
          )}
          <Button
            className="w-full"
            onClick={handleCreate}
            disabled={!name.trim() || create.isPending}
          >
            {create.isPending ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              "Create Rule"
            )}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function RuleRow({ rule }: { rule: PolicyRule }) {
  const del = useDeleteRule();
  const [confirmDelete, setConfirmDelete] = useState(false);

  return (
    <tr className="border-b hover:bg-muted/50">
      <td className="px-4 py-3 text-sm font-medium">{rule.name}</td>
      <td className="px-4 py-3 text-sm text-muted-foreground">{rule.scope}</td>
      <td className="px-4 py-3 font-mono text-sm">{rule.tool}</td>
      <td className="px-4 py-3">
        <Badge
          variant={rule.action === "allow" ? "secondary" : "destructive"}
        >
          {rule.action}
        </Badge>
      </td>
      <td className="px-4 py-3">
        {rule.constitutional ? (
          <Badge variant="outline" className="gap-1">
            <Lock className="h-3 w-3" />
            Constitutional
          </Badge>
        ) : (
          <Badge variant="outline">Mutable</Badge>
        )}
      </td>
      <td className="px-4 py-3">
        {!rule.constitutional && (
          <Button
            size="sm"
            variant="ghost"
            className="h-7 text-xs text-destructive"
            onClick={() => {
              if (!confirmDelete) {
                setConfirmDelete(true);
                return;
              }
              del.mutate(rule.name);
              setConfirmDelete(false);
            }}
            aria-label={`Delete rule ${rule.name}`}
          >
            {confirmDelete ? "Confirm?" : <Trash2 className="h-3 w-3" />}
          </Button>
        )}
      </td>
    </tr>
  );
}

function RulesTab() {
  const { data, isLoading, error } = usePolicyRules();
  const rules = data?.rules ?? [];

  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <CreateRuleDialog />
      </div>
      {error ? (
        <Card>
          <CardContent className="flex items-center gap-2 p-6 text-destructive">
            <AlertCircle className="h-5 w-5" />
            <p>Failed to load rules: {error.message}</p>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="p-0">
            <table className="w-full" aria-label="Policy rules">
              <thead>
                <tr className="border-b bg-muted/50">
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                    Name
                  </th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                    Scope
                  </th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                    Tool
                  </th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                    Action
                  </th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground">
                    Type
                  </th>
                  <th className="px-4 py-2 text-left text-xs font-medium text-muted-foreground"></th>
                </tr>
              </thead>
              <tbody>
                {isLoading ? (
                  <tr>
                    <td
                      colSpan={6}
                      className="px-4 py-8 text-center text-muted-foreground"
                    >
                      Loading...
                    </td>
                  </tr>
                ) : rules.length === 0 ? (
                  <tr>
                    <td
                      colSpan={6}
                      className="px-4 py-8 text-center text-muted-foreground"
                    >
                      No policy rules configured
                    </td>
                  </tr>
                ) : (
                  rules.map((r) => <RuleRow key={r.name} rule={r} />)
                )}
              </tbody>
            </table>
          </CardContent>
        </Card>
      )}
    </div>
  );
}

function ProposalsTab() {
  const [statusFilter, setStatusFilter] = useState<string | undefined>(
    "pending",
  );
  const { data, isLoading, error } = usePolicyProposals(statusFilter);
  const approve = useApproveProposal();
  const reject = useRejectProposal();
  const [reviewNote, setReviewNote] = useState<Record<string, string>>({});

  const proposals = data?.proposals ?? [];

  return (
    <div className="space-y-4">
      <div className="flex gap-2">
        {["pending", "approved", "rejected", undefined].map((s) => (
          <Button
            key={s ?? "all"}
            size="sm"
            variant={statusFilter === s ? "default" : "outline"}
            onClick={() => setStatusFilter(s)}
          >
            {s ?? "All"}
          </Button>
        ))}
      </div>

      {error ? (
        <Card>
          <CardContent className="flex items-center gap-2 p-6 text-destructive">
            <AlertCircle className="h-5 w-5" />
            <p>Failed to load proposals: {error.message}</p>
          </CardContent>
        </Card>
      ) : isLoading ? (
        <Card>
          <CardContent className="p-6 text-center text-muted-foreground">
            Loading...
          </CardContent>
        </Card>
      ) : proposals.length === 0 ? (
        <Card>
          <CardContent className="p-6 text-center text-muted-foreground">
            No {statusFilter ?? ""} proposals
          </CardContent>
        </Card>
      ) : (
        <div className="space-y-3">
          {proposals.map((p) => (
            <Card key={p.id}>
              <CardContent className="p-4 space-y-3">
                <div className="flex items-start justify-between">
                  <div>
                    <p className="font-medium">
                      {p.proposed_rule.name}{" "}
                      <Badge variant="outline" className="ml-2">
                        {p.proposed_rule.action} {p.proposed_rule.tool}
                      </Badge>
                    </p>
                    {p.agent_id && (
                      <p className="text-xs text-muted-foreground">
                        Proposed by: {p.agent_id}
                      </p>
                    )}
                  </div>
                  <Badge
                    variant={
                      p.status === "approved"
                        ? "secondary"
                        : p.status === "rejected"
                          ? "destructive"
                          : "outline"
                    }
                  >
                    {p.status}
                  </Badge>
                </div>
                <p className="text-sm">{p.justification}</p>
                {p.review_note && (
                  <p className="text-sm text-muted-foreground italic">
                    Review: {p.review_note}
                  </p>
                )}
                {p.status === "pending" && (
                  <div className="flex gap-2 items-center">
                    <Input
                      placeholder="Optional note..."
                      className="h-8 text-sm flex-1"
                      value={reviewNote[p.id] ?? ""}
                      onChange={(e) =>
                        setReviewNote((prev) => ({
                          ...prev,
                          [p.id]: e.target.value,
                        }))
                      }
                      aria-label="Review note"
                    />
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-8"
                      onClick={() =>
                        approve.mutate({
                          id: p.id,
                          note: reviewNote[p.id] || undefined,
                        })
                      }
                      disabled={approve.isPending}
                    >
                      <Check className="mr-1 h-3 w-3" />
                      Approve
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-8 text-destructive"
                      onClick={() =>
                        reject.mutate({
                          id: p.id,
                          note: reviewNote[p.id] || undefined,
                        })
                      }
                      disabled={reject.isPending}
                    >
                      <X className="mr-1 h-3 w-3" />
                      Reject
                    </Button>
                  </div>
                )}
              </CardContent>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}

export default function PolicyPage() {
  return (
    <div className="p-6 space-y-6">
      <div>
        <h1 className="text-2xl font-bold flex items-center gap-2">
          <Shield className="h-6 w-6" />
          Policy Management
        </h1>
        <p className="text-sm text-muted-foreground">
          Manage tool access rules and review AI-proposed policy changes.
        </p>
      </div>

      <Tabs defaultValue="rules">
        <TabsList>
          <TabsTrigger value="rules">Rules</TabsTrigger>
          <TabsTrigger value="proposals">Proposals</TabsTrigger>
        </TabsList>
        <TabsContent value="rules" className="mt-4">
          <RulesTab />
        </TabsContent>
        <TabsContent value="proposals" className="mt-4">
          <ProposalsTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
