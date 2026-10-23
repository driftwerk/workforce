import { useEffect, useRef, useState } from "react";
import { useParams } from "react-router";
import { useGamesStore } from "@/stores/games-store";
import { useAuthStore } from "@/stores/auth-store";
import { LobbyShell } from "@/components/games/lobby-shell";
import { ScoresCard } from "@/components/games/scores-card";
import { GameOver } from "@/components/games/game-over";
import { HostContinue } from "@/components/games/host-continue";
import type { GuessFeedRow } from "@/pages/games/types";

const CANVAS_W = 520;
const CANVAS_H = 400;

type Tool = "pen" | "eraser" | "fill";
const COLORS = [
  "#1a1a2e",
  "#ef4444",
  "#f97316",
  "#eab308",
  "#22c55e",
  "#3b82f6",
  "#a855f7",
  "#92400e",
  "#ec4899",
];
const SIZES = [3, 7, 14];

export function HumanPictionaryView() {
  const view = useGamesStore((s) => s.view);
  const playerId = useGamesStore((s) => s.playerId);
  const myWord = useGamesStore((s) => s.myWord);
  const myChoices = useGamesStore((s) => s.myChoices);
  const start = useGamesStore((s) => s.start);
  const guess = useGamesStore((s) => s.guess);
  const chooseWord = useGamesStore((s) => s.chooseWord);
  const requestWord = useGamesStore((s) => s.requestWord);
  const postDrawing = useGamesStore((s) => s.postDrawing);
  const nextDrawSeq = useGamesStore((s) => s.nextDrawSeq);
  const token = useAuthStore((s) => s.token);
  const { roomId } = useParams();

  if (!view || view.game.view !== "human_pictionary") return null;
  const g = view.game;
  const isHost = view.host_id === playerId;
  const isDrawer = g.drawer_id != null && g.drawer_id === playerId;
  // Drive the "you solved it" lock from the authoritative solver list, not the
  // chat feed (which scrolls past MAX_FEED and would flip the flag back).
  const iGuessed = g.correct.some((row) => row.player_id === playerId);
  const artUrl =
    g.art_round != null
      ? `/games/api/rooms/${roomId}/art?token=${encodeURIComponent(token ?? "")}&v=${g.snapshot}`
      : null;

  return (
    <>
      {view.phase === "lobby" && (
        <LobbyShell players={view.players} youId={playerId} isHost={isHost} onStart={start}>
          Each round one of you draws a secret word while everyone else — and the AI — races to guess
          it in the chat. The quicker you get it the more you score, and the artist scores for every
          teammate who guesses it. Share code{" "}
          <span className="font-black text-[var(--g-pop2)]">{view.room_id}</span> with your team! 🖌️
        </LobbyShell>
      )}

      {view.phase === "choosing" &&
        (isDrawer ? (
          <WordChooser
            choices={myChoices}
            chooseWord={chooseWord}
            requestWord={requestWord}
            round={view.round}
          />
        ) : (
          <section className="g-card p-10 text-center">
            <div className="g-pulse text-5xl">🎨</div>
            <p className="mt-2 font-semibold text-[var(--g-ink)]/60">
              <span className="font-black text-[var(--g-pop2)]">{g.drawer_label}</span> is picking a
              word…
            </p>
          </section>
        ))}

      {view.phase === "drawing" &&
        (isDrawer ? (
          <DrawerPad
            word={myWord}
            requestWord={requestWord}
            postDrawing={postDrawing}
            nextDrawSeq={nextDrawSeq}
            round={view.round}
            guesses={g.guesses}
            onChat={guess}
          />
        ) : (
          <section className="space-y-4">
            <div className="g-card p-4 text-center">
              <p className="font-semibold text-[var(--g-ink)]/60">
                ✏️ <span className="font-black text-[var(--g-pop2)]">{g.drawer_label}</span> is
                drawing — guess what it is!
              </p>
              {iGuessed && (
                <p className="mt-1 font-extrabold text-[var(--g-pop2)]">
                  You got it! 🎉 Hang out in the chat while the others finish.
                </p>
              )}
            </div>
            {artUrl ? (
              <div className="g-card p-3">
                <img src={artUrl} alt="The drawing" className="mx-auto w-full max-w-lg rounded-2xl" />
              </div>
            ) : (
              <div className="g-card p-10 text-center">
                <div className="g-pulse text-5xl">🎨</div>
                <p className="mt-2 font-semibold text-[var(--g-ink)]/60">
                  Waiting for the first strokes…
                </p>
              </div>
            )}
            <Chat
              guesses={g.guesses}
              onSend={guess}
              placeholder={iGuessed ? "Say something… 💬" : "Type your guess…"}
              sendLabel={iGuessed ? "Send 💬" : "Guess 🎯"}
            />
          </section>
        ))}

      {view.phase === "reveal" && (
        <section className="space-y-4">
          <div className="g-card p-6 text-center">
            {g.unavailable ? (
              <>
                <div className="text-5xl">🚫</div>
                <h2 className="g-display mt-2 text-2xl">Round skipped — the drawer left</h2>
              </>
            ) : (
              <>
                <p className="font-bold text-[var(--g-ink)]/60">The word was</p>
                <h2 className="g-display text-3xl">{g.secret}</h2>
                {artUrl && (
                  <img
                    src={artUrl}
                    alt={g.secret ?? "drawing"}
                    className="mx-auto mt-3 w-full max-w-xs rounded-2xl"
                  />
                )}
                {g.correct.length > 0 ? (
                  <div className="mt-4 space-y-1">
                    <p className="font-bold text-[var(--g-ink)]/60">Who got it</p>
                    {g.correct.map((row, idx) => (
                      <div
                        key={row.player_id}
                        className="flex items-center justify-center gap-2 font-extrabold"
                      >
                        <span>{idx === 0 ? "🏆" : row.is_ai ? "🤖" : row.avatar}</span>
                        <span>{row.label}</span>
                        <span className="text-[var(--g-pop2)]">+{row.points}</span>
                      </div>
                    ))}
                    {g.drawer_points > 0 && g.drawer_label && (
                      <p className="mt-1 text-sm font-bold text-[var(--g-ink)]/60">
                        ✏️ {g.drawer_label} earned +{g.drawer_points} for a guessable drawing
                      </p>
                    )}
                  </div>
                ) : (
                  <p className="mt-2 font-bold text-[var(--g-ink)]/60">Nobody guessed it 😅</p>
                )}
              </>
            )}
          </div>
          <ScoresCard rows={view.scoreboard} youId={playerId} />
          <HostContinue isHost={isHost} round={view.round} totalRounds={view.total_rounds} />
        </section>
      )}

      {view.phase === "game_over" && <GameOver scoreboard={view.scoreboard} youId={playerId} />}
    </>
  );
}

function WordChooser({
  choices,
  chooseWord,
  requestWord,
  round,
}: {
  choices: string[] | null;
  chooseWord: (word: string) => void;
  requestWord: () => void;
  round: number;
}) {
  // Fetch the private shortlist when the turn opens (clearing any stale one).
  useEffect(() => {
    requestWord();
  }, [round, requestWord]);

  return (
    <section className="g-card space-y-5 p-7 text-center">
      <p className="font-semibold text-[var(--g-ink)]/60">Your turn — pick a word to draw ✏️</p>
      {choices && choices.length > 0 ? (
        <div className="flex flex-wrap justify-center gap-3">
          {choices.map((word) => (
            <button key={word} className="g-btn px-6 py-3 text-lg" onClick={() => chooseWord(word)}>
              {word}
            </button>
          ))}
        </div>
      ) : (
        <div className="g-pulse text-4xl">🎲</div>
      )}
      <p className="text-xs font-semibold text-[var(--g-ink)]/50">
        Pick quickly — dawdle and one gets chosen for you! ⏳
      </p>
    </section>
  );
}

function DrawerPad({
  word,
  requestWord,
  postDrawing,
  nextDrawSeq,
  round,
  guesses,
  onChat,
}: {
  word: string | null;
  requestWord: () => void;
  postDrawing: (blob: Blob, round: number, seq: number) => void;
  nextDrawSeq: () => number;
  round: number;
  guesses: GuessFeedRow[];
  onChat: (text: string) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const drawing = useRef(false);
  const dirty = useRef(false);
  const [shared, setShared] = useState(false);
  const [color, setColor] = useState("#1a1a2e");
  const [size, setSize] = useState(7);
  const [tool, setTool] = useState<Tool>("pen");
  const flashTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Snapshot history for undo/redo (capped to bound memory).
  const history = useRef<ImageData[]>([]);
  const histIdx = useRef(-1);
  const [canUndo, setCanUndo] = useState(false);
  const [canRedo, setCanRedo] = useState(false);

  function ctx2d() {
    return canvasRef.current?.getContext("2d", { willReadFrequently: true }) ?? null;
  }

  function syncHistButtons() {
    setCanUndo(histIdx.current > 0);
    setCanRedo(histIdx.current < history.current.length - 1);
  }

  // Capture the canvas after a committed action (drops any redo branch).
  function commit() {
    const ctx = ctx2d();
    if (!ctx) return;
    const snap = ctx.getImageData(0, 0, CANVAS_W, CANVAS_H);
    history.current = history.current.slice(0, histIdx.current + 1);
    history.current.push(snap);
    const MAX = 25;
    if (history.current.length > MAX) history.current.shift();
    histIdx.current = history.current.length - 1;
    syncHistButtons();
  }

  function restore(idx: number) {
    const ctx = ctx2d();
    const snap = history.current[idx];
    if (ctx && snap) {
      ctx.putImageData(snap, 0, 0);
      dirty.current = true;
    }
  }

  function undo() {
    if (histIdx.current <= 0) return;
    histIdx.current -= 1;
    restore(histIdx.current);
    syncHistButtons();
  }

  function redo() {
    if (histIdx.current >= history.current.length - 1) return;
    histIdx.current += 1;
    restore(histIdx.current);
    syncHistButtons();
  }

  function flashShared() {
    setShared(true);
    if (flashTimer.current) clearTimeout(flashTimer.current);
    flashTimer.current = setTimeout(() => setShared(false), 1500);
  }
  useEffect(() => {
    return () => {
      if (flashTimer.current) clearTimeout(flashTimer.current);
    };
  }, []);

  // Ask for the secret word (and reset the canvas) at the start of each round.
  useEffect(() => {
    requestWord();
    dirty.current = false;
    const ctx = ctx2d();
    if (ctx) {
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, CANVAS_W, CANVAS_H);
      ctx.lineCap = "round";
      ctx.lineJoin = "round";
    }
    // Reset the undo history with the blank canvas as the baseline.
    history.current = [];
    histIdx.current = -1;
    commit();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [round, requestWord]);

  // Throttled snapshot upload while the canvas is dirty.
  useEffect(() => {
    const tick = () => {
      if (!dirty.current) return;
      const seq = nextDrawSeq();
      canvasRef.current?.toBlob((blob) => blob && postDrawing(blob, round, seq), "image/png");
      dirty.current = false;
    };
    const timer = setInterval(tick, 5000);
    return () => clearInterval(timer);
  }, [postDrawing, nextDrawSeq, round]);

  function point(e: React.PointerEvent<HTMLCanvasElement>) {
    const c = canvasRef.current!;
    const r = c.getBoundingClientRect();
    return {
      x: (e.clientX - r.left) * (c.width / r.width),
      y: (e.clientY - r.top) * (c.height / r.height),
    };
  }

  function down(e: React.PointerEvent<HTMLCanvasElement>) {
    const ctx = canvasRef.current?.getContext("2d");
    if (!ctx) return;
    const p = point(e);
    if (tool === "fill") {
      floodFill(ctx, Math.round(p.x), Math.round(p.y), color);
      dirty.current = true;
      commit();
      return;
    }
    drawing.current = true;
    // Per-stroke style so colour/size/eraser changes apply immediately.
    ctx.strokeStyle = tool === "eraser" ? "#ffffff" : color;
    ctx.lineWidth = tool === "eraser" ? size * 2.5 : size;
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.beginPath();
    ctx.moveTo(p.x, p.y);
    canvasRef.current?.setPointerCapture(e.pointerId);
  }

  function move(e: React.PointerEvent<HTMLCanvasElement>) {
    if (!drawing.current) return;
    const ctx = canvasRef.current?.getContext("2d");
    if (!ctx) return;
    const p = point(e);
    ctx.lineTo(p.x, p.y);
    ctx.stroke();
    dirty.current = true;
  }

  function up() {
    if (!drawing.current) return;
    drawing.current = false;
    commit();
  }

  function clear() {
    const ctx = ctx2d();
    if (ctx) {
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, CANVAS_W, CANVAS_H);
      dirty.current = true;
      commit();
    }
  }

  function done() {
    const seq = nextDrawSeq();
    canvasRef.current?.toBlob((blob) => {
      if (blob) {
        postDrawing(blob, round, seq);
        flashShared();
      }
    }, "image/png");
    dirty.current = false;
  }

  return (
    <section className="space-y-4">
      <div className="g-card space-y-3 p-5 text-center">
        <p className="font-semibold text-[var(--g-ink)]/60">You're drawing — your word is</p>
        <h2 className="g-display text-3xl">{word ?? "…"}</h2>

        <div className="flex flex-wrap items-center justify-center gap-1.5">
          {COLORS.map((c) => (
            <button
              key={c}
              type="button"
              aria-label={`colour ${c}`}
              onClick={() => {
                setColor(c);
                setTool("pen");
              }}
              className={`h-7 w-7 rounded-full border-2 ${
                tool !== "eraser" && color === c
                  ? "border-[var(--g-pop2)] ring-2 ring-[var(--g-pop2)] ring-offset-1"
                  : "border-black/15"
              }`}
              style={{ background: c }}
            />
          ))}
        </div>

        <div className="flex flex-wrap items-center justify-center gap-2">
          <div className="flex items-center gap-1">
            {SIZES.map((s) => (
              <button
                key={s}
                type="button"
                aria-label={`brush ${s}`}
                onClick={() => setSize(s)}
                className={`grid h-8 w-8 place-items-center rounded-xl ${
                  size === s ? "g-tile-chosen" : "g-tile"
                }`}
              >
                <span
                  className="rounded-full bg-[var(--g-ink)]"
                  style={{ width: s + 2, height: s + 2 }}
                />
              </button>
            ))}
          </div>
          {(
            [
              ["pen", "✏️ Pen"],
              ["eraser", "🧽 Eraser"],
              ["fill", "🪣 Fill"],
            ] as [Tool, string][]
          ).map(([t, label]) => (
            <button
              key={t}
              type="button"
              onClick={() => setTool(t)}
              className={`g-tile px-3 py-1 text-sm font-extrabold ${tool === t ? "g-tile-chosen" : ""}`}
            >
              {label}
            </button>
          ))}
        </div>

        <canvas
          ref={canvasRef}
          width={CANVAS_W}
          height={CANVAS_H}
          className="mx-auto w-full max-w-lg touch-none rounded-2xl border-2 border-[var(--g-pop2)]/20 bg-white"
          onPointerDown={down}
          onPointerMove={move}
          onPointerUp={up}
          onPointerLeave={up}
        />
        <div className="flex flex-wrap justify-center gap-2">
          <button className="g-btn-ghost px-4 disabled:opacity-40" onClick={undo} disabled={!canUndo}>
            ↶ Undo
          </button>
          <button className="g-btn-ghost px-4 disabled:opacity-40" onClick={redo} disabled={!canRedo}>
            ↷ Redo
          </button>
          <button className="g-btn-ghost px-5" onClick={clear}>
            Clear
          </button>
          <button
            className={`px-6 ${shared ? "g-btn-mint" : "g-btn"}`}
            onClick={done}
            disabled={shared}
          >
            {shared ? "Shared! ✓" : "Send it ✏️"}
          </button>
        </div>
        <p className="text-xs font-semibold text-[var(--g-ink)]/50">
          Everyone sees your drawing as you go — tap <b>Send it</b> to push the latest right away.
        </p>
      </div>

      {/* The drawer watches the guesses roll in and can cheer the room on. */}
      <Chat guesses={guesses} onSend={onChat} placeholder="Cheer them on — no spoilers! 🤫" sendLabel="Send 💬" />
    </section>
  );
}

function Chat({
  guesses,
  onSend,
  placeholder,
  sendLabel,
}: {
  guesses: GuessFeedRow[];
  onSend: (text: string) => void;
  placeholder: string;
  sendLabel: string;
}) {
  const [draft, setDraft] = useState("");
  const listRef = useRef<HTMLDivElement>(null);

  // Keep the newest line in view by scrolling the chat's OWN container — never
  // `scrollIntoView`, which scrolls the page and yanks the canvas under the
  // drawer's pen. Keyed on the last row's identity (not just length) so it still
  // fires once the feed saturates its cap.
  const last = guesses[guesses.length - 1];
  useEffect(() => {
    const el = listRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [guesses.length, last?.player_id, last?.text, last?.correct]);

  function send() {
    const text = draft.trim();
    if (text) {
      onSend(text);
      setDraft("");
    }
  }

  return (
    <div className="g-card space-y-2 p-3">
      <p className="font-bold text-[var(--g-ink)]/60">Chat</p>
      <div ref={listRef} className="max-h-56 space-y-1 overflow-y-auto">
        {guesses.length === 0 ? (
          <p className="text-sm font-semibold text-[var(--g-ink)]/50">
            No messages yet — start guessing! 💬
          </p>
        ) : (
          guesses.map((row, idx) => (
            <div
              key={idx}
              className={`flex items-center gap-2 text-sm font-semibold ${
                row.correct ? "text-[var(--g-pop2)]" : "text-[var(--g-ink)]/70"
              }`}
            >
              <span>{row.is_ai ? "🤖" : row.avatar}</span>
              <span className="font-extrabold">{row.label}:</span>
              {/* Never echo a correct guess's text — it's the secret word. */}
              <span>{row.correct ? "guessed the word! 🎉" : row.text}</span>
              {row.correct ? (
                <span className="ml-auto">✅</span>
              ) : row.solved ? (
                <span className="ml-auto text-xs font-bold text-[var(--g-ink)]/40">✓</span>
              ) : null}
            </div>
          ))
        )}
      </div>
      <div className="flex gap-2">
        <input
          className="flex-1 rounded-2xl border-2 border-[var(--g-pop2)]/20 bg-white p-3 text-lg font-semibold outline-none focus:border-[var(--g-pop)]"
          maxLength={60}
          placeholder={placeholder}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") send();
          }}
        />
        <button className="g-btn px-5" onClick={send} disabled={!draft.trim()}>
          {sendLabel}
        </button>
      </div>
    </div>
  );
}

function hexToRgb(hex: string): [number, number, number] {
  const h = hex.replace("#", "");
  return [parseInt(h.slice(0, 2), 16), parseInt(h.slice(2, 4), 16), parseInt(h.slice(4, 6), 16)];
}

// Stack-based flood fill with a small colour tolerance (so anti-aliased edges
// don't leave a halo). A `visited` map keeps it from re-scanning pixels.
function floodFill(ctx: CanvasRenderingContext2D, x: number, y: number, fill: string) {
  const w = ctx.canvas.width;
  const h = ctx.canvas.height;
  if (x < 0 || y < 0 || x >= w || y >= h) return;
  const img = ctx.getImageData(0, 0, w, h);
  const data = img.data;
  const start = y * w + x;
  const ti = start * 4;
  const tr = data[ti]!;
  const tg = data[ti + 1]!;
  const tb = data[ti + 2]!;
  const ta = data[ti + 3]!;
  const [fr, fg, fb] = hexToRgb(fill);
  if (tr === fr && tg === fg && tb === fb && ta === 255) return;
  const tol = 40 * 40;
  const matches = (i: number) => {
    const dr = data[i]! - tr;
    const dg = data[i + 1]! - tg;
    const db = data[i + 2]! - tb;
    const da = data[i + 3]! - ta;
    return dr * dr + dg * dg + db * db + da * da <= tol;
  };
  const visited = new Uint8Array(w * h);
  const stack = [start];
  visited[start] = 1;
  while (stack.length) {
    const p = stack.pop() as number;
    const i = p * 4;
    if (!matches(i)) continue;
    data[i] = fr;
    data[i + 1] = fg;
    data[i + 2] = fb;
    data[i + 3] = 255;
    const px = p % w;
    const py = (p - px) / w;
    const push = (n: number) => {
      if (!visited[n]) {
        visited[n] = 1;
        stack.push(n);
      }
    };
    if (px > 0) push(p - 1);
    if (px < w - 1) push(p + 1);
    if (py > 0) push(p - w);
    if (py < h - 1) push(p + w);
  }
  ctx.putImageData(img, 0, 0);
}
