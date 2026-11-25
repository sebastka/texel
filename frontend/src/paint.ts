// Paints pictures on a worker's canvas (which has the size it has on screen,
// in device pixels), scaled to fill it, with netplay's hidden half over it.
//
// With WebGL 2: the PlayStation's picture as its VRAM holds it (15-bit
// colours), read straight from the emulator's memory and unpacked by a
// shader, so that nothing converts a frame to RGBA or copies it on the way;
// RGBA pictures (the PS2's, the PlayStation's 24-bit ones) uploaded as they
// are. Without WebGL 2, a 2D canvas, from RGBA only.

import type { Half } from "./protocol";

/** Where a picture's 15-bit pixels are in a module's memory, as
 * `Emulator.picture` and `Replayer.picture` give it: `[address (bytes),
 * length (pixels), stride, x, y, w, h]`. */
export type PictureLayout = Uint32Array;

type Viewport = { width: number; height: number };

/** Paints on a canvas: see `painter`. */
export interface Painter {
  /** An RGBA picture. */
  paint(pixels: Uint8ClampedArray<ArrayBuffer>, w: number, h: number, viewport: Viewport, smooth: boolean, hidden: Half | null | undefined): void;
  /** Whether `paint15` works (WebGL 2). */
  readonly takes15: boolean;
  /** A 15-bit picture in `memory`, at `layout`. */
  paint15(memory: ArrayBuffer, layout: PictureLayout, viewport: Viewport, smooth: boolean, hidden: Half | null | undefined): void;
  /** The last picture at its own size, as PNG (a save state's screenshot). */
  screenshot(): Promise<Blob | null>;
}

/** A painter for `canvas`: WebGL 2 where the browser has it, else 2D. */
export function painter(canvas: OffscreenCanvas): Painter {
  const gl = canvas.getContext("webgl2", { alpha: false, antialias: false, depth: false, stencil: false });
  return gl ? new GlPainter(canvas, gl) : new Painter2d(canvas);
}

function fitCanvas(canvas: OffscreenCanvas, viewport: Viewport) {
  if (canvas.width !== viewport.width || canvas.height !== viewport.height) {
    canvas.width = viewport.width;
    canvas.height = viewport.height;
  }
}

/** The hidden half's rectangle in a canvas `cw` by `ch`, from its top left,
 * in whole pixels. */
function hiddenRect(hidden: Half, cw: number, ch: number): [number, number, number, number] {
  const [hw, hh] = [Math.round(cw / 2), Math.round(ch / 2)];
  return hidden === "top"
    ? [0, 0, cw, hh]
    : hidden === "bottom"
      ? [0, hh, cw, ch - hh]
      : hidden === "left"
        ? [0, 0, hw, ch]
        : [hw, 0, cw - hw, ch];
}

/** The hidden half, `w` by `h` from the 2D context's origin (`full`: the
 * canvas's height, for the text's size). */
function drawHidden(ctx: OffscreenCanvasRenderingContext2D, w: number, h: number, full: number) {
  ctx.fillStyle = "#16181d";
  ctx.fillRect(0, 0, w, h);
  ctx.fillStyle = "#6b7280";
  ctx.font = `${Math.round(full / 28)}px system-ui, sans-serif`;
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillText("The other player's half", w / 2, h / 2);
}

/** Through a staging canvas at the picture's size, scaled to the canvas. */
class Painter2d implements Painter {
  readonly takes15 = false;
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

  paint(pixels: Uint8ClampedArray<ArrayBuffer>, w: number, h: number, viewport: Viewport, smooth: boolean, hidden: Half | null | undefined) {
    const { canvas, ctx, staging } = this;
    if (w === 0 || h === 0) return;
    if (staging.width !== w || staging.height !== h) {
      staging.width = w;
      staging.height = h;
    }
    this.stagingCtx.putImageData(new ImageData(pixels, w, h), 0, 0);
    fitCanvas(canvas, viewport);
    // Resizing resets the context state: set it on every frame.
    ctx.imageSmoothingEnabled = smooth;
    ctx.imageSmoothingQuality = "high";
    // The picture always fills the screen, whatever the resolution.
    ctx.drawImage(staging, 0, 0, canvas.width, canvas.height);
    if (hidden) {
      const [x, y, rw, rh] = hiddenRect(hidden, canvas.width, canvas.height);
      ctx.save();
      ctx.translate(x, y);
      drawHidden(ctx, rw, rh, canvas.height);
      ctx.restore();
    }
  }

  paint15(): void {
    throw new Error("a 2D canvas paints RGBA pictures only");
  }

  screenshot(): Promise<Blob | null> {
    return this.staging.convertToBlob({ type: "image/png" });
  }
}

// A triangle covering the target, the texture's first row at its top.
const VERTEX = `#version 300 es
out vec2 uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

// The console's colours, 5 bits a channel widened to 8 as it shows them,
// (c << 3) | (c >> 2): from the 15-bit texture to the RGBA one, pixel for
// pixel (the first row at row 0, as uploaded).
const UNPACK = `#version 300 es
precision highp float;
precision highp usampler2D;
uniform usampler2D src;
out vec4 color;
uint widen(uint c) { return ((c & 31u) << 3) | ((c & 31u) >> 2); }
void main() {
  uint c = texelFetch(src, ivec2(gl_FragCoord.xy), 0).r;
  color = vec4(float(widen(c)), float(widen(c >> 5)), float(widen(c >> 10)), 255.0) / 255.0;
}`;

const SHOW = `#version 300 es
precision highp float;
uniform sampler2D picture;
in vec2 uv;
out vec4 color;
void main() { color = texture(picture, uv); }`;

function compile(gl: WebGL2RenderingContext, fragment: string): WebGLProgram {
  const program = gl.createProgram();
  for (const [type, source] of [
    [gl.VERTEX_SHADER, VERTEX],
    [gl.FRAGMENT_SHADER, fragment],
  ] as const) {
    const shader = gl.createShader(type);
    if (!shader) throw new Error("WebGL made no shader");
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader) ?? "a shader failed");
    gl.attachShader(program, shader);
  }
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program) ?? "a program failed");
  return program;
}

/**
 * Two textures the picture's size: the 15-bit one (integers, as uploaded),
 * which a shader unpacks into the RGBA one, then drawn to the canvas
 * (filtered when smooth, through mipmaps when shrunk). RGBA pictures go to
 * the RGBA texture directly. The samplers use texture unit 0, their
 * default.
 */
class GlPainter implements Painter {
  readonly takes15 = true;
  private readonly unpack: WebGLProgram;
  private readonly show: WebGLProgram;
  private readonly framebuffer: WebGLFramebuffer;
  private readonly overlay: WebGLTexture;
  /** The 15-bit texture and the RGBA one, and their sizes. */
  private src: WebGLTexture | null = null;
  private srcSize: [number, number] = [0, 0];
  private picture: WebGLTexture | null = null;
  private size: [number, number] = [0, 0];
  /** What the overlay texture shows. */
  private overlayKey = "";

  constructor(
    readonly canvas: OffscreenCanvas,
    private readonly gl: WebGL2RenderingContext,
  ) {
    this.unpack = compile(gl, UNPACK);
    this.show = compile(gl, SHOW);
    this.framebuffer = gl.createFramebuffer();
    this.overlay = gl.createTexture();
    gl.bindVertexArray(gl.createVertexArray());
  }

  /** The RGBA texture, `w` by `h` (made again when the size changes),
   * bound; it is the framebuffer's. */
  private bindPicture(w: number, h: number): void {
    const { gl } = this;
    if (this.picture && this.size[0] === w && this.size[1] === h) {
      gl.bindTexture(gl.TEXTURE_2D, this.picture);
      return;
    }
    gl.deleteTexture(this.picture);
    this.picture = gl.createTexture();
    this.size = [w, h];
    gl.bindTexture(gl.TEXTURE_2D, this.picture);
    gl.texStorage2D(gl.TEXTURE_2D, Math.floor(Math.log2(Math.max(w, h))) + 1, gl.RGBA8, w, h);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.framebuffer);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.picture, 0);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  }

  /** The 15-bit texture, `w` by `h`, bound. */
  private bindSrc(w: number, h: number): void {
    const { gl } = this;
    if (this.src && this.srcSize[0] === w && this.srcSize[1] === h) {
      gl.bindTexture(gl.TEXTURE_2D, this.src);
      return;
    }
    gl.deleteTexture(this.src);
    this.src = gl.createTexture();
    this.srcSize = [w, h];
    gl.bindTexture(gl.TEXTURE_2D, this.src);
    gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R16UI, w, h);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  }

  paint(pixels: Uint8ClampedArray<ArrayBuffer>, w: number, h: number, viewport: Viewport, smooth: boolean, hidden: Half | null | undefined) {
    const { gl } = this;
    if (w === 0 || h === 0) return;
    this.bindPicture(w, h);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array(pixels.buffer, pixels.byteOffset, w * h * 4));
    this.present(viewport, smooth, hidden);
  }

  paint15(memory: ArrayBuffer, layout: PictureLayout, viewport: Viewport, smooth: boolean, hidden: Half | null | undefined) {
    const { gl } = this;
    const [address = 0, length = 0, stride = 0, x = 0, y = 0, w = 0, h = 0] = layout;
    if (w === 0 || h === 0) return;
    this.bindPicture(w, h);
    this.bindSrc(w, h);
    // The picture's rectangle in VRAM's rows, from the emulator's memory as
    // it is now (no copy).
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 2);
    gl.pixelStorei(gl.UNPACK_ROW_LENGTH, stride);
    gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, x);
    gl.pixelStorei(gl.UNPACK_SKIP_ROWS, y);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, w, h, gl.RED_INTEGER, gl.UNSIGNED_SHORT, new Uint16Array(memory, address, length));
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 4);
    gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
    gl.pixelStorei(gl.UNPACK_SKIP_PIXELS, 0);
    gl.pixelStorei(gl.UNPACK_SKIP_ROWS, 0);
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.framebuffer);
    gl.viewport(0, 0, w, h);
    gl.useProgram(this.unpack);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    this.present(viewport, smooth, hidden);
  }

  /** The RGBA texture drawn to the canvas, filling it. */
  private present(viewport: Viewport, smooth: boolean, hidden: Half | null | undefined) {
    const { gl, canvas } = this;
    fitCanvas(canvas, viewport);
    gl.bindTexture(gl.TEXTURE_2D, this.picture);
    const shrunk = this.size[0] > canvas.width || this.size[1] > canvas.height;
    if (smooth && shrunk) gl.generateMipmap(gl.TEXTURE_2D);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, smooth ? (shrunk ? gl.LINEAR_MIPMAP_LINEAR : gl.LINEAR) : gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, smooth ? gl.LINEAR : gl.NEAREST);
    gl.viewport(0, 0, canvas.width, canvas.height);
    gl.useProgram(this.show);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    if (hidden) this.drawHidden(hidden);
  }

  /** Netplay's hidden half: drawn on a 2D canvas (when it changes), then
   * as a texture over the picture. */
  private drawHidden(hidden: Half) {
    const { gl, canvas } = this;
    const [x, y, w, h] = hiddenRect(hidden, canvas.width, canvas.height);
    gl.bindTexture(gl.TEXTURE_2D, this.overlay);
    const key = `${hidden} ${w} ${h} ${canvas.height}`;
    if (key !== this.overlayKey) {
      const text = new OffscreenCanvas(w, h);
      const ctx = text.getContext("2d");
      if (!ctx) return;
      drawHidden(ctx, w, h, canvas.height);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, text);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      this.overlayKey = key;
    }
    // WebGL counts the canvas's rows from the bottom.
    gl.viewport(x, canvas.height - y - h, w, h);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }

  screenshot(): Promise<Blob | null> {
    const { gl } = this;
    const [w, h] = this.size;
    if (!this.picture || w === 0 || h === 0) return Promise.resolve(null);
    // The RGBA texture's rows, the picture's top first (as uploaded).
    const pixels = new Uint8ClampedArray(w * h * 4);
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.framebuffer);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    const shot = new OffscreenCanvas(w, h);
    shot.getContext("2d")?.putImageData(new ImageData(pixels, w, h), 0, 0);
    return shot.convertToBlob({ type: "image/png" });
  }
}
