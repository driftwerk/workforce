import { useState } from "react";
import { readStored, writeStored } from "@/lib/local-store";
import { isCards, parseWindow } from "@/lib/pipeline/columns";

/** A viewer's own choices for one pipeline's board, kept in this browser. */
export interface BoardPrefs {
  /** Cards per column; absent: the definition's. */
  cards?: number;
  /** Column id → window spelling; absent: the definition's. */
  windows: Record<string, string>;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parsed(raw: string | null): Record<string, unknown> {
  try {
    const value: unknown = raw === null ? null : JSON.parse(raw);
    return isRecord(value) ? value : {};
  } catch {
    return {};
  }
}

/** Stored preferences as the board can use them: a malformed value is dropped, so the hub is never sent a spelling it refuses. */
export function readBoardPrefs(raw: string | null): BoardPrefs {
  const { cards, windows } = parsed(raw);
  const valid = Object.fromEntries(
    Object.entries(isRecord(windows) ? windows : {}).filter(
      (entry): entry is [string, string] => typeof entry[1] === "string" && parseWindow(entry[1]) !== null,
    ),
  );
  return isCards(cards) ? { cards, windows: valid } : { windows: valid };
}

/**
 * The viewer's choices for the board at `scope/id`, read again when the
 * pipeline changes. Handing a setter `undefined` removes the choice, so the
 * definition's value applies again.
 */
export function usePipelineBoardPrefs(scope: string, id: string) {
  const key = `pipeline-board:${scope}/${id}`;
  const [state, setState] = useState(() => ({ key, prefs: readBoardPrefs(readStored(key)) }));
  if (state.key !== key) setState({ key, prefs: readBoardPrefs(readStored(key)) });

  const update = (prefs: BoardPrefs) => {
    setState({ key, prefs });
    writeStored(key, JSON.stringify(prefs));
  };
  const { windows } = state.prefs;

  return {
    prefs: state.prefs,
    setCards: (cards: number | undefined) => update(cards === undefined ? { windows } : { cards, windows }),
    setWindow: (column: string, window: string | undefined) => {
      const others = Object.entries(windows).filter(([id]) => id !== column);
      update({
        ...state.prefs,
        windows: Object.fromEntries(window === undefined ? others : [...others, [column, window]]),
      });
    },
  };
}
