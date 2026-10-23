import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";

interface Props {
  label: string;
  value: string | number;
  isLoading?: boolean;
}

export function StatCard({ label, value, isLoading }: Props) {
  return (
    <Card>
      <CardContent className="pt-6">
        <div className="text-xs text-muted-foreground">{label}</div>
        <div className="mt-1 text-2xl font-semibold tabular-nums">
          {isLoading ? <Skeleton className="h-8 w-24" /> : value}
        </div>
      </CardContent>
    </Card>
  );
}
