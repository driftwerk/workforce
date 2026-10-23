import { Badge } from "@/components/ui/badge";
import { definitionReach } from "@/lib/access";

/** How far past its owner a definition reaches, shown only when it reaches at
 *  all: a listing marks what widens the private default, not the default. */
export function ReachBadge({
  visibility,
  className,
}: {
  visibility: string | undefined;
  className?: string;
}) {
  const reach = definitionReach(visibility);
  return reach ? (
    <Badge variant="outline" className={className}>
      {reach}
    </Badge>
  ) : null;
}
