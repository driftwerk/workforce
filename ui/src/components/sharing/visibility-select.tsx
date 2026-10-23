import { Globe, Lock, Users, type LucideIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  VISIBILITIES,
  visibilityHelp,
  visibilityLabel,
  type Visibility,
} from "@/lib/access";

const ICONS: Record<Visibility, LucideIcon> = {
  private: Lock,
  team: Users,
  everyone: Globe,
};

export function visibilityIcon(visibility: Visibility): LucideIcon {
  return ICONS[visibility];
}

/** Choose who reaches an owned resource, one of three: each option carries the
 *  reach it grants, because the middle one is the only ambiguous word. */
export function VisibilitySelect({
  value,
  options = VISIBILITIES,
  disabled,
  className,
  onChange,
}: {
  value: Visibility;
  /** The reaches this resource may be given; the full three by default. */
  options?: Visibility[];
  disabled?: boolean;
  className?: string;
  onChange: (next: Visibility) => void;
}) {
  const Current = visibilityIcon(value);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button size="sm" variant="outline" className={className} disabled={disabled}>
          <Current aria-hidden className="h-3.5 w-3.5" />
          {visibilityLabel(value)}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-w-72">
        <DropdownMenuRadioGroup
          value={value}
          onValueChange={(next) => onChange(next as Visibility)}
        >
          {options.map((option) => {
            const Icon = visibilityIcon(option);
            return (
              <DropdownMenuRadioItem key={option} value={option} className="items-start">
                <span className="flex flex-col gap-0.5">
                  <span className="flex items-center gap-1.5">
                    <Icon aria-hidden className="h-3.5 w-3.5" />
                    {visibilityLabel(option)}
                  </span>
                  <span className="text-[10px] text-muted-foreground">
                    {visibilityHelp(option)}
                  </span>
                </span>
              </DropdownMenuRadioItem>
            );
          })}
        </DropdownMenuRadioGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
