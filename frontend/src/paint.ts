// Paints an RGBA picture on a worker's 2D canvas: through a staging canvas
// at the picture's size, scaled to the canvas (which has the size it has on
// screen, in device pixels); and netplay's hidden half over it.

import type { Half } from "./protocol";

export class Painter {
  private readonly ctx: OffscreenCanvasRenderingContext2D;
  private readonly staging = new OffscreenCanvas(1, 1);
  private readonly stagingCtx: OffscreenCanvasRenderingContext2D;

  constructor(readonly canvas: OffscreenCanvas) {
    const ctx = canvas.getContext("2d", { alpha: false });
    const stagingCtx = this.staging.getContext("2d", { alpha: false });
    if (!ctx || !stagingCtx) throw new Error("no 2D context for the canvas");
    this.ctx = ctx;
    this.stagingCtx = stagingCtx;
  }

  /** The last picture, as painted at its own size (for screenshots). */
  get picture(): OffscreenCanvas {
    return this.staging;
  }

  paint(
    pixels: Uint8ClampedArray<ArrayBuffer>,
    w: number,
    h: number,
    viewport: { width: number; height: number },
    smooth: boolean,
    hidden: Half | null | undefined,
  ) {
    const { canvas, ctx, staging } = this;
    if (w === 0 || h === 0) return;
    if (staging.width !== w || staging.height !== h) {
      staging.width = w;
      staging.height = h;
    }
    this.stagingCtx.putImageData(new ImageData(pixels, w, h), 0, 0);
    if (canvas.width !== viewport.width || canvas.height !== viewport.height) {
      canvas.width = viewport.width;
      canvas.height = viewport.height;
    }
    // Resizing resets the context state: set it on every frame.
    ctx.imageSmoothingEnabled = smooth;
    ctx.imageSmoothingQuality = "high";
    // The picture always fills the screen, whatever the resolution.
    ctx.drawImage(staging, 0, 0, canvas.width, canvas.height);
    if (hidden) {
      const [cw, ch] = [canvas.width, canvas.height];
      const [x, y, rw, rh] =
        hidden === "top"
          ? [0, 0, cw, ch / 2]
          : hidden === "bottom"
            ? [0, ch / 2, cw, ch / 2]
            : hidden === "left"
              ? [0, 0, cw / 2, ch]
              : [cw / 2, 0, cw / 2, ch];
      ctx.fillStyle = "#16181d";
      ctx.fillRect(x, y, rw, rh);
      ctx.fillStyle = "#6b7280";
      ctx.font = `${Math.round(ch / 28)}px system-ui, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText("The other player's half", x + rw / 2, y + rh / 2);
    }
  }
}
