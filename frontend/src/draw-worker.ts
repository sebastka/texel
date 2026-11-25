// The PlayStation's scaled picture, drawn in its own worker (another core):
// the emulation worker records its drawing (Emulator.takeDrawing) and sends
// it here, where a Replayer draws it in software and paints the canvas.
// Each message carries every recording since the last one acknowledged:
// none is dropped (they change what is drawn next), only pictures are.

import init, { Replayer } from "./wasm/texel_web.js";
import { type Painter, painter as makePainter } from "./paint";
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
/** This module's memory, where the replayer's pictures are. */
let memory: WebAssembly.Memory | null = null;

async function draw(msg: Extract<ToDrawer, { type: "draw" }>) {
  if (!replayer || !painter) return;
  const started = performance.now();
  for (const d of msg.drawings) replayer.apply(new Uint8Array(d));
  // The 15-bit picture, unconverted, where the painter takes it (WebGL);
  // else RGBA (24-bit pictures, a 2D canvas).
  const layout = painter.takes15 && memory ? replayer.picture() : undefined;
  if (layout && memory) painter.paint15(memory.buffer, layout, msg.viewport, msg.smooth, msg.hidden);
  else {
    replayer.render();
    const pixels = replayer.frame() as Uint8ClampedArray<ArrayBuffer>;
    painter.paint(pixels, replayer.frameWidth, replayer.frameHeight, msg.viewport, msg.smooth, msg.hidden);
  }
  const ms = performance.now() - started;
  const screenshot = msg.screenshot ? await painter.screenshot() : null;
  scope.postMessage({ type: "drawn", ms, screenshot } satisfies FromDrawer);
}

scope.onmessage = (e: MessageEvent<ToDrawer>) => {
  const msg = e.data;
  if (msg.type === "start") {
    void init().then((wasm) => {
      memory = wasm.memory;
      replayer = new Replayer();
      painter = makePainter(msg.canvas);
      scope.postMessage({ type: "drawn", ms: 0, screenshot: null } satisfies FromDrawer);
    });
    return;
  }
  draw(msg).catch((err: unknown) => scope.postMessage({ type: "error", message: String(err) } satisfies FromDrawer));
};
