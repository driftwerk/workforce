import {
  useDeferredValue,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Check, ChevronsUpDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
} from "@/components/ui/command";
import { cn } from "@/lib/utils";

export interface ComboboxOption {
  value: string;
  label: string;
  sub?: string;
  /** Render the primary line in monospace (tool names, ids). */
  mono?: boolean;
}

/** Check + primary label + muted secondary line — the shared item layout. */
export function ComboboxItemRow({
  checked,
  label,
  sub,
  mono,
}: {
  checked: boolean;
  label: string;
  sub?: string;
  mono?: boolean;
}) {
  return (
    <>
      <Check
        className={cn("mr-2 h-4 w-4 shrink-0", checked ? "opacity-100" : "opacity-0")}
      />
      <div className="min-w-0">
        <div className={cn("truncate", mono && "font-mono text-xs")}>{label}</div>
        {sub && <div className="truncate text-xs text-muted-foreground">{sub}</div>}
      </div>
    </>
  );
}

/** Cap rendered options so a large catalogue (e.g. the tool list) can't flood
 * the DOM; the search box narrows past the cap. */
const MAX_OPTIONS = 50;

function filterOptions(
  options: ComboboxOption[],
  query: string,
): { visible: ComboboxOption[]; hidden: number } {
  const q = query.trim().toLowerCase();
  const matched = q
    ? options.filter((o) =>
        `${o.label} ${o.sub ?? ""} ${o.value}`.toLowerCase().includes(q),
      )
    : options;
  return {
    visible: matched.slice(0, MAX_OPTIONS),
    hidden: Math.max(0, matched.length - MAX_OPTIONS),
  };
}

/** Anchored dropdown: a trigger that opens a search panel in place (closes on
 * outside-click / Escape). No modal — every combobox surface looks the same. */
function ComboboxPopover({
  id,
  triggerLabel,
  placeholder,
  open,
  setOpen,
  children,
}: {
  id?: string;
  triggerLabel: string;
  placeholder: string;
  open: boolean;
  setOpen: (open: boolean) => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open, setOpen]);

  return (
    <div className="relative" ref={ref}>
      <Button
        type="button"
        id={id}
        variant="outline"
        className="w-full justify-between font-normal"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <span className={cn("truncate", !triggerLabel && "text-muted-foreground")}>
          {triggerLabel || placeholder}
        </span>
        <ChevronsUpDown className="ml-2 h-4 w-4 shrink-0 opacity-50" />
      </Button>
      {open && (
        <div className="absolute inset-x-0 z-50 mt-1 rounded-md border bg-popover shadow-md">
          {children}
        </div>
      )}
    </div>
  );
}

interface ComboboxFrameProps {
  surface: "dialog" | "inline";
  triggerLabel: string;
  placeholder: string;
  searchPlaceholder: string;
  id?: string;
  caption?: string;
  /** Resolved empty-state label (loading / error / no-match). */
  empty: string;
  /** No options after filtering — show the empty state. */
  isEmpty: boolean;
  query: string;
  setQuery: (query: string) => void;
  open: boolean;
  setOpen: (open: boolean) => void;
  children: ReactNode;
}

/** Trigger + searchable list, rendered either as an anchored dropdown
 * (`dialog`) or an always-open inline panel (`inline`, used inside a form). We
 * filter ourselves (`shouldFilter={false}` + a controlled, deferred query), so
 * only matching items render. */
function ComboboxFrame({
  surface,
  triggerLabel,
  placeholder,
  searchPlaceholder,
  id,
  caption,
  empty,
  isEmpty,
  query,
  setQuery,
  open,
  setOpen,
  children,
}: ComboboxFrameProps) {
  const list = (autoFocus: boolean) => (
    <>
      <CommandInput
        placeholder={searchPlaceholder}
        value={query}
        onValueChange={setQuery}
        autoFocus={autoFocus}
      />
      <CommandList>
        {isEmpty && <CommandEmpty>{empty}</CommandEmpty>}
        {children}
      </CommandList>
    </>
  );

  if (surface === "inline") {
    return (
      <div className="space-y-1.5">
        {caption && <div className="text-xs text-muted-foreground">{caption}</div>}
        <Command className="rounded-md border" shouldFilter={false} loop>
          {list(false)}
        </Command>
      </div>
    );
  }

  return (
    <ComboboxPopover
      id={id}
      triggerLabel={triggerLabel}
      placeholder={placeholder}
      open={open}
      setOpen={setOpen}
    >
      <Command shouldFilter={false} loop>
        {list(true)}
      </Command>
    </ComboboxPopover>
  );
}

interface CommonProps {
  options: ComboboxOption[];
  isLoading?: boolean;
  isError?: boolean;
  errorMessage?: string;
  placeholder?: string;
  searchPlaceholder?: string;
  emptyLabel?: string;
  surface?: "dialog" | "inline";
  id?: string;
}

function emptyLabelFor(
  isError: boolean | undefined,
  isLoading: boolean | undefined,
  errorMessage: string | undefined,
  emptyLabel: string,
): string {
  if (isError) return errorMessage ?? "Couldn't load options.";
  if (isLoading) return "Loading…";
  return emptyLabel;
}

/** Single-select combobox. */
export function Combobox({
  options,
  value,
  onChange,
  placeholder = "Select…",
  searchPlaceholder = "Search…",
  emptyLabel = "No matches.",
  surface = "dialog",
  isLoading,
  isError,
  errorMessage,
  id,
}: CommonProps & {
  value: string;
  onChange: (value: string, option: ComboboxOption) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const deferredQuery = useDeferredValue(query);
  const { visible, hidden } = useMemo(
    () => filterOptions(options, deferredQuery),
    [options, deferredQuery],
  );
  const selected = options.find((o) => o.value === value);
  const setOpenAndReset = (o: boolean) => {
    setOpen(o);
    if (!o) setQuery("");
  };
  return (
    <ComboboxFrame
      surface={surface}
      triggerLabel={selected?.label ?? value}
      caption={surface === "inline" && selected ? `Selected: ${selected.label}` : undefined}
      placeholder={placeholder}
      searchPlaceholder={searchPlaceholder}
      empty={emptyLabelFor(isError, isLoading, errorMessage, emptyLabel)}
      isEmpty={visible.length === 0}
      query={query}
      setQuery={setQuery}
      open={open}
      setOpen={setOpenAndReset}
      id={id}
    >
      <CommandGroup>
        {visible.map((o) => (
          <CommandItem
            key={o.value}
            value={o.value}
            onSelect={() => {
              onChange(o.value, o);
              setOpenAndReset(false);
            }}
          >
            <ComboboxItemRow checked={value === o.value} label={o.label} sub={o.sub} mono={o.mono} />
          </CommandItem>
        ))}
      </CommandGroup>
      {hidden > 0 && (
        <div className="px-3 py-1.5 text-xs text-muted-foreground">
          +{hidden} more — refine your search
        </div>
      )}
    </ComboboxFrame>
  );
}

/** Multi-select combobox — selecting toggles membership and keeps the list open. */
export function MultiCombobox({
  options,
  values,
  onChange,
  placeholder = "Select…",
  searchPlaceholder = "Search…",
  emptyLabel = "No matches.",
  surface = "dialog",
  isLoading,
  isError,
  errorMessage,
  id,
}: CommonProps & {
  values: string[];
  onChange: (values: string[]) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const deferredQuery = useDeferredValue(query);
  const { visible, hidden } = useMemo(
    () => filterOptions(options, deferredQuery),
    [options, deferredQuery],
  );
  const selected = new Set(values);
  const triggerLabel = values
    .map((v) => options.find((o) => o.value === v)?.label ?? v)
    .join(", ");
  const setOpenAndReset = (o: boolean) => {
    setOpen(o);
    if (!o) setQuery("");
  };
  return (
    <ComboboxFrame
      surface={surface}
      triggerLabel={triggerLabel}
      placeholder={placeholder}
      searchPlaceholder={searchPlaceholder}
      empty={emptyLabelFor(isError, isLoading, errorMessage, emptyLabel)}
      isEmpty={visible.length === 0}
      query={query}
      setQuery={setQuery}
      open={open}
      setOpen={setOpenAndReset}
      id={id}
    >
      <CommandGroup>
        {visible.map((o) => (
          <CommandItem
            key={o.value}
            value={o.value}
            onSelect={() =>
              onChange(
                selected.has(o.value)
                  ? values.filter((v) => v !== o.value)
                  : [...values, o.value],
              )
            }
          >
            <ComboboxItemRow checked={selected.has(o.value)} label={o.label} sub={o.sub} mono={o.mono} />
          </CommandItem>
        ))}
      </CommandGroup>
      {hidden > 0 && (
        <div className="px-3 py-1.5 text-xs text-muted-foreground">
          +{hidden} more — refine your search
        </div>
      )}
    </ComboboxFrame>
  );
}
