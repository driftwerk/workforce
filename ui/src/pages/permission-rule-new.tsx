import { useState } from "react";
import { Link, useNavigate } from "react-router";
import { ArrowLeft, Shield } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useAuthStore } from "@/stores/auth-store";
import { useCreatePermissionRule } from "@/hooks/use-permissions";
import {
  RuleForm,
  splitApprovers,
  type RuleFormInitial,
  type RuleFormValues,
} from "@/components/permissions/rule-form";

export default function PermissionRuleNewPage() {
  const navigate = useNavigate();
  const identity = useAuthStore((s) => s.identity);
  // Token-only login (no identity) = static gateway token = admin. Email login
  // checks the role, mirroring the sidebar's admin gate.
  const isAdmin = identity ? identity.role === "system_admin" : true;

  const create = useCreatePermissionRule();
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // Re-seed values for a partial failure: the full form (scope, state,
  // approvers) is preserved and only the patterns that didn't land remain, so a
  // re-submit retries just those without silently reverting scope/state.
  // `formKey` remounts the form so it picks the new seed up.
  const [retryInitial, setRetryInitial] = useState<RuleFormInitial | null>(null);
  const [formKey, setFormKey] = useState(0);

  if (identity && !isAdmin) {
    navigate("/", { replace: true });
    return null;
  }

  const handleSubmit = async (values: RuleFormValues) => {
    setError(null);
    setSubmitting(true);
    // Each picked pattern is its own rule; the server upserts on
    // (scope, scope_id, tool_pattern) so re-submitting failures is safe.
    // RuleForm already normalises approvers/self-approval for the chosen state.
    const results = await Promise.allSettled(
      values.patterns.map((tool_pattern) =>
        create.mutateAsync({
          scope: values.scope,
          scope_id: values.scopeId,
          tool_pattern,
          state: values.state,
          approvers: values.approvers,
          allow_self_approval: values.allowSelfApproval,
        }),
      ),
    );
    setSubmitting(false);
    const failed = values.patterns
      .map((pattern, i) => ({ pattern, result: results[i] }))
      .filter((x) => x.result?.status === "rejected");
    if (failed.length === 0) {
      navigate("/permissions?tab=rules");
      return;
    }
    const split = splitApprovers(values.approvers);
    setRetryInitial({
      scope: values.scope,
      scopeId: values.scopeId,
      scopeLabel: values.scopeLabel,
      patterns: failed.map((f) => f.pattern),
      state: values.state,
      approverUserIds: split.userIds,
      approverRoles: split.roles,
      allowSelfApproval: values.allowSelfApproval,
    });
    setFormKey((k) => k + 1);
    const detail = failed
      .map((f) => {
        const reason = (f.result as PromiseRejectedResult).reason as
          | Error
          | undefined;
        return `${f.pattern}: ${reason?.message ?? "failed"}`;
      })
      .join("; ");
    setError(
      `${values.patterns.length - failed.length} created, ${failed.length} failed — ${detail}`,
    );
  };

  return (
    <div className="mx-auto max-w-2xl space-y-6 p-6">
      <div className="flex items-center gap-3">
        <Button
          variant="ghost"
          size="icon"
          asChild
          aria-label="Back to permissions"
        >
          <Link to="/permissions?tab=rules">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex items-center gap-2">
          <Shield className="h-5 w-5" />
          <h1 className="text-xl font-semibold">New permission rule</h1>
        </div>
      </div>

      <p className="text-sm text-muted-foreground">
        Rules are an optional fine-grained overlay on top of the always-on
        capability matrix. They only take effect when the rules layer is enabled
        (<span className="font-mono">[permissions] enabled</span>). Pick who the
        rule applies to, which tool calls it covers, and how agents should treat
        it.
      </p>

      <Card>
        <CardContent className="p-6">
          <RuleForm
            key={formKey}
            mode="create"
            initial={retryInitial ?? undefined}
            submitLabel="Create"
            submitting={submitting}
            error={error}
            onSubmit={handleSubmit}
            onCancel={() => navigate("/permissions?tab=rules")}
          />
        </CardContent>
      </Card>
    </div>
  );
}
