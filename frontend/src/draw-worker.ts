// The PlayStation's scaled picture, drawn in its own worker (another core):
// the emulation worker records its drawing (Emulator.takeDrawing) and sends
// it here, where a Replayer draws it in software and paints the canvas.
// Each message carries every recording since the last one acknowledged:
// none is dropped (they change what is drawn next), only pictures are.

import init, { Replayer } from "./wasm/texel_web.js";
import { Painter } from "./paint";
import type { Half } from "./protocol";

export type ToDrawer =
  | { type: "start"; canvas: OffscreenCanvas }
  | {
      type: "draw";
      drawings: ArrayBuffer[];
      viewport: { width: number; height: number };
      smooth: boolean;
      hidden: Half | null;
      /** A screenshot wanted (PNG): for a save state. */
      screenshot: boolean;
    };

/** "drawn": ready for more; `ms`: what drawing took. */
export type FromDrawer =
  | { type: "drawn"; ms: number; screenshot: Blob | null }
  | { type: "error"; message: string };

const scope = self as unknown as DedicatedWorkerGlobalScope;
let replayer: Replayer | null = null;
let painter: Painter | null = null;

async function draw(msg: Extract<ToDrawer, { type: "draw" }>) {
  if (!replayer || !painter) return;
  const started = performance.now();
  for (const d of msg.drawings) replayer.apply(new Uint8Array(d));
  replayer.render();
  const pixels = replayer.frame() as Uint8ClampedArray<ArrayBuffer>;
  painter.paint(pixels, replayer.frameWidth, replayer.frameHeight, msg.viewport, msg.smooth, msg.hidden);
  const ms = performance.now() - started;
  const screenshot = msg.screenshot ? await painter.picture.convertToBlob({ type: "image/png" }) : null;
  scope.postMessage({ type: "drawn", ms, screenshot } satisfies FromDrawer);
}

scope.onmessage = (e: MessageEvent<ToDrawer>) => {
  const msg = e.data;
  if (msg.type === "start") {
    void init().then(() => {
      replayer = new Replayer();
      painter = new Painter(msg.canvas);
      scope.postMessage({ type: "drawn", ms: 0, screenshot: null } satisfies FromDrawer);
    });
    return;
  }
  draw(msg).catch((err: unknown) => scope.postMessage({ type: "error", message: String(err) } satisfies FromDrawer));
};
