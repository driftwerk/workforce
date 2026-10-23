import { Badge } from "@/components/ui/badge";
import type { PrincipalKind } from "@/lib/api/cost";

export interface PrincipalDisplay {
  id: string;
  display_name?: string | null;
  kind?: PrincipalKind | null;
}

interface Props {
  principal: PrincipalDisplay | null | undefined;
  placeholder?: string;
}

/**
 * Display a principal. Falls back to the raw `id` when no `display_name`
 * is set, mirroring the backend's `identity_display::resolve_principal`
 * rule so the wire format never has to repeat the fallback.
 */
export function PrincipalPill({ principal, placeholder = "—" }: Props) {
  if (!principal) {
    return <span className="text-muted-foreground">{placeholder}</span>;
  }
  const label = principal.display_name?.trim() || principal.id;
  const variant = principal.kind === "user" ? "secondary" : "outline";
  return (
    <Badge variant={variant} className="text-xs">
      {label}
    </Badge>
  );
}
