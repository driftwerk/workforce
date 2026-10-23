import { create } from "zustand";
import { useAuthStore } from "@/stores/auth-store";
import type { RoomView, ServerGameMessage } from "@/pages/games/types";

let ws: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let pingInterval: ReturnType<typeof setInterval> | null = null;
let reconnectAttempts = 0;
// Ever-increasing snapshot sequence for drawing uploads so the server can drop
// any that arrive out of order. It never resets: the server zeroes its per-round
// counter and only ever stores a seq we sent, so this always stays ahead of it —
// even across a reconnect.
let drawSeq = 0;

function wsUrl(roomId: string): string {
  const token = useAuthStore.getState().token ?? "";
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}/games/ws?token=${encodeURIComponent(
    token,
  )}&room=${encodeURIComponent(roomId)}`;
}

function send(message: Record<string, unknown>) {
  if (ws?.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(message));
  }
}

function cleanup() {
  if (pingInterval) clearInterval(pingInterval);
  if (reconnectTimer) clearTimeout(reconnectTimer);
  pingInterval = null;
  reconnectTimer = null;
  if (ws) {
    ws.onopen = ws.onclose = ws.onerror = ws.onmessage = null;
    if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) {
      ws.close();
    }
    ws = null;
  }
}

interface GamesState {
  connected: boolean;
  connecting: boolean;
  roomId: string | null;
  playerId: string | null;
  view: RoomView | null;
  error: string | null;
  myWord: string | null;
  myChoices: string[] | null;
  connect: (roomId: string) => void;
  disconnect: () => void;
  start: () => void;
  submit: (text: string) => void;
  vote: (target: string) => void;
  guess: (text: string) => void;
  chooseWord: (word: string) => void;
  skipReveal: () => void;
  requestWord: () => void;
  nextDrawSeq: () => number;
  postDrawing: (blob: Blob, round: number, seq: number) => void;
  dismissError: () => void;
}

export const useGamesStore = create<GamesState>((set) => ({
  connected: false,
  connecting: false,
  roomId: null,
  playerId: null,
  view: null,
  error: null,
  myWord: null,
  myChoices: null,

  connect: (roomId: string) => {
    cleanup();
    reconnectAttempts = 0;
    set({
      roomId,
      connecting: true,
      connected: false,
      view: null,
      playerId: null,
      error: null,
      myWord: null,
      myChoices: null,
    });
    openSocket(roomId);
  },

  disconnect: () => {
    cleanup();
    reconnectAttempts = 0;
    set({ connected: false, connecting: false, roomId: null, view: null, playerId: null });
  },

  start: () => send({ type: "start" }),
  submit: (text: string) => send({ type: "submit", text }),
  vote: (target: string) => send({ type: "vote", target }),
  guess: (text: string) => send({ type: "guess", text }),
  chooseWord: (word: string) => send({ type: "choose_word", word }),
  skipReveal: () => send({ type: "continue" }),
  // Clear both first so a stale word/shortlist from a previous round never flashes;
  // the server answers with whichever the drawer currently needs.
  requestWord: () => {
    set({ myWord: null, myChoices: null });
    send({ type: "request_word" });
  },
  // Stamp the sequence at capture time (before encoding), so two overlapping
  // snapshots can never have their order inverted by `toBlob` resolving late.
  nextDrawSeq: () => ++drawSeq,
  postDrawing: (blob: Blob, round: number, seq: number) => {
    const { roomId } = useGamesStore.getState();
    const token = useAuthStore.getState().token ?? "";
    if (!roomId) return;
    const query = new URLSearchParams({ token, round: String(round), seq: String(seq) });
    void fetch(`/games/api/rooms/${encodeURIComponent(roomId)}/drawing?${query}`, {
      method: "POST",
      headers: { "Content-Type": "image/png" },
      body: blob,
    }).catch(() => {});
  },
  dismissError: () => set({ error: null }),
}));

function openSocket(roomId: string) {
  ws = new WebSocket(wsUrl(roomId));

  ws.onopen = () => {
    reconnectAttempts = 0;
    useGamesStore.setState({ connected: true, connecting: false });
    pingInterval = setInterval(() => send({ type: "ping" }), 30_000);
  };

  ws.onclose = () => {
    useGamesStore.setState({ connected: false, connecting: false });
    if (pingInterval) clearInterval(pingInterval);
    pingInterval = null;
    scheduleReconnect(roomId);
  };

  ws.onerror = () => {};

  ws.onmessage = (event) => {
    try {
      handle(JSON.parse(event.data) as ServerGameMessage);
    } catch {
      /* ignore malformed frames */
    }
  };
}

function scheduleReconnect(roomId: string) {
  if (useGamesStore.getState().roomId !== roomId) return;
  reconnectAttempts++;
  const delay = Math.min(1000 * 2 ** (reconnectAttempts - 1), 30_000);
  reconnectTimer = setTimeout(() => {
    if (useGamesStore.getState().roomId === roomId) {
      useGamesStore.setState({ connecting: true });
      openSocket(roomId);
    }
  }, delay);
}

function handle(msg: ServerGameMessage) {
  switch (msg.type) {
    case "welcome":
      useGamesStore.setState({ playerId: msg.player_id });
      break;
    case "state": {
      const { type: _type, ...view } = msg;
      useGamesStore.setState({ view });
      break;
    }
    case "error":
      useGamesStore.setState({ error: msg.message });
      break;
    case "your_word":
      useGamesStore.setState({ myWord: msg.word });
      break;
    case "word_choices":
      useGamesStore.setState({ myChoices: msg.words });
      break;
    case "pong":
      break;
  }
}
