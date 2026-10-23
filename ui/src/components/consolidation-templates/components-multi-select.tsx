import { ChevronDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuCheckboxItem,
  DropdownMenuLabel,
} from "@/components/ui/dropdown-menu";

interface Props {
  /** Selectable canonical `line` labels. */
  options: string[];
  selected: string[];
  onChange: (next: string[]) => void;
}

/** Multi-select of the canonical lines a subtotal sums (`components`). */
export function ComponentsMultiSelect({ options, selected, onChange }: Props) {
  const toggle = (label: string) => {
    onChange(
      selected.includes(label)
        ? selected.filter((l) => l !== label)
        : [...selected, label],
    );
  };

  const summary =
    selected.length === 0
      ? "Sums…"
      : selected.length === 1
        ? selected[0]
        : `${selected.length} lines`;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="outline"
          size="sm"
          className="h-8 min-w-[8rem] max-w-[14rem] justify-between text-xs font-normal"
        >
          <span className="truncate">{summary}</span>
          <ChevronDown className="h-3.5 w-3.5 ml-1 shrink-0 opacity-60" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-72 overflow-y-auto">
        <DropdownMenuLabel className="text-xs">Sum these lines</DropdownMenuLabel>
        {options.length === 0 ? (
          <div className="px-2 py-1.5 text-xs text-muted-foreground">
            No lines to sum yet.
          </div>
        ) : (
          options.map((label) => (
            <DropdownMenuCheckboxItem
              key={label}
              checked={selected.includes(label)}
              // Keep the menu open while toggling several.
              onSelect={(e) => e.preventDefault()}
              onCheckedChange={() => toggle(label)}
              className="text-xs"
            >
              {label}
            </DropdownMenuCheckboxItem>
          ))
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
