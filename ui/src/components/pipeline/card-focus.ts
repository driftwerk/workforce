/** An item's card on the board, found by its record's document id so opening and closing its panel keeps it in view and in focus. */

const ATTRIBUTE = "data-pipeline-item";

/** The attribute that marks the card for `docId`. */
export function cardProps(docId: string): Record<string, string> {
  return { [ATTRIBUTE]: docId };
}

/** The card on the board for `docId`, wherever the item sits now; null when the board does not show it. */
export function cardFor(docId: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(`[${ATTRIBUTE}="${CSS.escape(docId)}"]`);
}

/** Scrolls `card` fully into view inside its column, moving nothing already in view. */
export function revealCard(card: HTMLElement): void {
  card.scrollIntoView({ block: "nearest", inline: "nearest" });
}
