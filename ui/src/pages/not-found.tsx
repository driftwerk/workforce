import { AlertTriangle } from "lucide-react";
import { EmptyState } from "@/components/empty-state";
import { Link } from "react-router";
import { Button } from "@/components/ui/button";

export default function NotFoundPage() {
  return (
    <div className="p-6">
      <EmptyState icon={AlertTriangle} title="404 — Not Found" description="The page you're looking for doesn't exist." />
      <Button asChild variant="outline"><Link to="/">Back to Dashboard</Link></Button>
    </div>
  );
}
