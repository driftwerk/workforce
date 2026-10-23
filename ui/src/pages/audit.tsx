import { Shield } from "lucide-react";
import { EmptyState } from "@/components/empty-state";

export default function AuditPage() {
  return (
    <div className="p-6">
      <EmptyState icon={Shield} title="Audit Log" description="View audit trail and activity history." />
    </div>
  );
}
