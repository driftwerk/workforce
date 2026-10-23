import { useState } from "react";
import { ExternalLink } from "lucide-react";
import { formatRelativeTime } from "@/lib/utils";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import type { PipelineDefinition } from "@/lib/pipeline/definition";
import { allEvents, itemId, itemLink, positionAt, summary } from "@/lib/pipeline/lineage";
import type { Item } from "./board";
import { cardFor, revealCard } from "./card-focus";
import { Feed } from "./feed";
import { ItemCostsPanel } from "./item-costs";
import { ItemTimingsPanel } from "./item-timings";

export interface ItemPanelProps {
  scope: string;
  id: string;
  def: PipelineDefinition;
  itemLabel: string;
  /** The record of the item to show, as the board last held it; `null` closes the panel. */
  item: Item | null;
  canViewCosts: boolean;
  onClose: () => void;
}

export function ItemPanel({ scope, id, def, itemLabel, item, canViewCosts, onClose }: ItemPanelProps) {
  // The last item shown stays rendered while the dialog animates closed.
  const [shown, setShown] = useState(item);
  if (item && item !== shown) setShown(item);

  return (
    <Dialog open={item !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        className="flex max-h-[85vh] max-w-4xl flex-col"
        onCloseAutoFocus={(event) => {
          const card = shown && cardFor(shown.id);
          if (!card) return;
          event.preventDefault();
          card.focus({ preventScroll: true });
          revealCard(card);
        }}
      >
        {shown && (
          <ItemDetail scope={scope} id={id} def={def} itemLabel={itemLabel} item={shown} canViewCosts={canViewCosts} />
        )}
      </DialogContent>
    </Dialog>
  );
}

function ItemDetail({
  scope,
  id,
  def,
  itemLabel,
  item,
  canViewCosts,
}: Omit<ItemPanelProps, "item" | "onClose"> & { item: Item }) {
  const text = summary(def, item.body);
  const link = itemLink(def, item.body);
  const column = positionAt(def, item.body, null)?.label;
  return (
    <>
      <DialogHeader className="pr-6">
        <DialogTitle className="flex flex-wrap items-baseline gap-x-2 font-mono">
          {itemId(item.body, item.id)}
          {column && <span className="font-sans text-sm font-normal text-muted-foreground">{column}</span>}
        </DialogTitle>
        <DialogDescription asChild>
          <div className="space-y-1">
            {text && <p>{text}</p>}
            <p className="flex flex-wrap items-center gap-x-3 text-xs">
              {link && (
                <a
                  href={link.url}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center gap-0.5 font-mono hover:text-foreground"
                >
                  {link.label}
                  <ExternalLink className="h-3 w-3" />
                </a>
              )}
              <span>updated {formatRelativeTime(item.body.updated_at)}</span>
            </p>
          </div>
        </DialogDescription>
      </DialogHeader>
      <div className="-mx-6 min-h-0 space-y-4 overflow-y-auto px-6">
        <ItemTimingsPanel scope={scope} id={id} item={item} itemLabel={itemLabel} />
        {canViewCosts && <ItemCostsPanel scope={scope} id={id} item={item} itemLabel={itemLabel} />}
        <Feed events={allEvents(def, [item])} at={null} />
      </div>
    </>
  );
}
