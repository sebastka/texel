// The emulator, in a dedicated worker: it runs frames, draws them on the
// transferred canvas, feeds the audio ring and saves memory cards.

import init, { Emulator } from "./wasm/texel_web.js";
// Types only: the PS2's module is imported when a PS2 session starts.
import type { Ps2Emulator } from "./wasm/texel_web_ps2.js";
import { RING_FRAMES, Ring } from "./audio";
import type { FromDrawer, ToDrawer } from "./draw-worker";
import { OPS_BYTES, Pipe, RECORDS_BYTES, Vu1Link } from "./vu1-pipe";
import type { ToVu1 } from "./vu1-worker";
import { type Painter, painter as makePainter } from "./paint";
import { Session } from "./netplay";
import { cardUrl } from "./api";
import type { Axes, CodeSet, FromWorker, RoomSnapshot, Slots, StartMessage, ToWorker } from "./protocol";

const scope = self as unknown as DedicatedWorkerGlobalScope;
const post = (msg: FromWorker) => scope.postMessage(msg);

/** Audio kept queued: enough to ride out a slow frame, little latency. */
const AUDIO_TARGET = 3 * 1024;
const MEMCARD_SLOTS = [0, 1];
/** How often memory cards the game wrote go to the server: soon after a
 * save, so that closing the page loses nothing. */
const CARDS_EVERY_MS = 2000;

let emu: Emulator | null = null;
/** Drawing on the GPU (the PS2's GS, the PlayStation's scaled drawing):
 * frames go to the canvas through WebGPU. */
let gpu: { present(width: number, height: number, smooth: boolean, hidden: number): void; readonly renderer: string | undefined } | null =
  null;
/** What is compiled to WebAssembly (the PS2's EE and VUs, the PlayStation's
 * CPU), for the player's status; null: all interpreted. */
let compiled: string | null = null;
/** A PS2 game's disc is in the drive (inserted by its own path). */
let ps2Disc = false;
let ring: Ring | null = null;
/** Paints pictures drawn here (software, without a drawing worker). */
let painter: Painter | null = null;
/** The PlayStation's module's memory, where its pictures are (`picture`);
 * null for the PS2. */
let ps1Memory: WebAssembly.Memory | null = null;
/** The PlayStation's scaled picture drawn in another worker (see
 * draw-worker.ts): it, whether it is ready for more, the recordings not
 * sent yet, a screenshot awaited. */
let drawer: Worker | null = null;
let drawerReady = false;
let drawings: ArrayBuffer[] = [];
let screenshotWanted: ((b: Blob | null) => void) | null = null;
/** Resolution: as chosen, and as drawn (lower while emulation lags). */
let chosenScale = 1;
let autoScale = 1;
/** Milliseconds per frame, averaged: emulating it, drawing it elsewhere. */
let emuMs = 0;
let drawMs = 0;
/** Seconds running behind (or with room to spare), and until when not to
 * change the resolution again. */
let behindFor = 0;
let roomFor = 0;
let holdUntil = 0;
let canvas: OffscreenCanvas | null = null;
/** The frame at its native size, scaled from there onto `canvas`. */
let viewport = { width: 640, height: 480 };
let smooth = false;
let gameId: string | null = null;
/** The user's memory cards in the two slots (ids; null: a blank card,
 * not kept). */
let cards: Slots = [null, null];
/** Recorded with save states, which load only with it. */
let biosId = "";
/** The memory cards are this user's, and saved: not a netplay guest's
 * copies of the host's. */
let ownCards = false;
/** Netplay: the room this player joins once its emulator is ready, and
 * the room's messages that came before. */
let pendingRoom: RoomSnapshot | null = null;
let netQueue: (string | ArrayBuffer)[] = [];
let paused = false;
/** Fast-forward (not in netplay): frames back to back, no sound. */
let fast = false;
/** The page's audio plays (browsers wait for a click): frames follow it. */
let audioRunning = false;
let timer: ReturnType<typeof setTimeout> | undefined;
let nextFrameAt = 0;
let frames = 0;
let cardsCheckedAt = 0;
let statsAt = 0;
/** Netplay session (the room's game), if any. */
let net: Session | null = null;
/** Local controllers, applied before each frame (or sent, in netplay). */
const local: { buttons: [number, number]; axes: [Axes, Axes] } = {
  buttons: [0, 0],
  axes: [
    [128, 128, 128, 128],
    [128, 128, 128, 128],
  ],
};

/** In step with a room (or waiting to be): no fast-forward then. */
const inStep = () => net !== null && (net.lockstep || net.waiting);

/** Runs the game in step with the room (see netplay.ts). */
function joinRoom(room: RoomSnapshot) {
  net?.close();
  net = new Session(room, {
    emu: () => emu,
    runFrame,
    status: (status) => post({ type: "net", status }),
    send: (data) => scope.postMessage({ type: "net-out", data } satisfies FromWorker, typeof data === "string" ? [] : [data.buffer]),
    sendDatagram: (data) => scope.postMessage({ type: "net-out", data, datagram: true } satisfies FromWorker, [data.buffer]),
    gzip: (data, decompress = false) => compress(data, "gzip", decompress),
    onStarted: () => draw(),
  });
  net.input = { buttons: local.buttons[0], axes: local.axes[0] };
  for (const data of netQueue) net.receive(data);
  netQueue = [];
}

/**
 * Reads bytes of a disc image file. Synchronous: the core reads sectors in
 * the middle of an instruction. Allowed in workers; the core caches chunks.
 * The chunk after each is read ahead, in the background: games stream their
 * discs, and a chunk read when needed stalls emulation as long as the
 * server takes (it may unzip the image).
 */
function readFile(file: number, offset: number, length: number): Uint8Array {
  const key = `${gameId ?? ""} ${file} ${offset} ${length}`;
  const ready = readAhead.get(key);
  readAhead.delete(key);
  const data = ready ?? readNow(file, offset, length);
  readNext(file, offset + length, length);
  return data;
}

/** Chunks read ahead (see `readFile`), by game, file, offset and length;
 * and those under way. */
const readAhead = new Map<string, Uint8Array>();
const readingAhead = new Set<string>();
/** Chunks read ahead and not asked for are dropped past this many. */
const READ_AHEAD_KEPT = 8;

function readNext(file: number, offset: number, length: number) {
  const id = gameId ?? "";
  const key = `${id} ${file} ${offset} ${length}`;
  if (readAhead.has(key) || readingAhead.has(key)) return;
  readingAhead.add(key);
  // Past the file's end, or at its shorter last chunk: nothing kept (the
  // core reads it when it needs it).
  fetch(`/api/games/${encodeURIComponent(id)}/files/${file}`, {
    headers: { Range: `bytes=${offset}-${offset + length - 1}` },
  })
    .then(async (resp) => {
      const data = new Uint8Array(await resp.arrayBuffer());
      if (resp.status !== 206 || data.length !== length) return;
      readAhead.set(key, data);
      // The oldest first (a Map keeps insertion order).
      for (const old of readAhead.keys()) {
        if (readAhead.size <= READ_AHEAD_KEPT) break;
        readAhead.delete(old);
      }
    })
    .catch(() => {})
    .finally(() => readingAhead.delete(key));
}

function readNow(file: number, offset: number, length: number): Uint8Array {
  const xhr = new XMLHttpRequest();
  xhr.open("GET", `/api/games/${encodeURIComponent(gameId ?? "")}/files/${file}`, false);
  xhr.responseType = "arraybuffer";
  xhr.setRequestHeader("Range", `bytes=${offset}-${offset + length - 1}`);
  xhr.send();
  if (xhr.status !== 206 && xhr.status !== 200) {
    throw new Error(`reading file ${file} at ${offset}: HTTP ${xhr.status}`);
  }
  const data = new Uint8Array(xhr.response as ArrayBuffer);
  // A server ignoring Range sends the whole file.
  return xhr.status === 200 ? data.subarray(offset, offset + length) : data;
}

async function fetchBytes(url: string): Promise<Uint8Array | null> {
  const resp = await fetch(url, { cache: "no-store" });
  if (resp.status === 404) return null;
  if (!resp.ok) throw new Error(`${url}: HTTP ${resp.status}`);
  return new Uint8Array(await resp.arrayBuffer());
}

/** The SBI file of a disc protected by LibCrypt: the subchannel data its
 * protection checks, which disc images do not keep. */
function fetchSbi(id: string): Promise<Uint8Array | null> {
  return fetchBytes(`/api/games/${encodeURIComponent(id)}/sbi`);
}

/** What the console runs on, for the player's status: drawing on the GPU,
 * code compiled. */
function renderer(): string | null {
  const parts = [gpu ? (gpu.renderer ? `WebGPU (${gpu.renderer})` : "WebGPU") : null, compiled];
  const shown = parts.filter((p) => p !== null);
  return shown.length > 0 ? shown.join(" · ") : null;
}

/** Whether this browser has WebGPU, with an adapter: asked before the
 * canvas is given to it, since a canvas has one kind of context for good. */
async function webgpu(): Promise<boolean> {
  const gpu = (navigator as unknown as { gpu?: { requestAdapter(): Promise<unknown> } }).gpu;
  if (!gpu) return false;
  try {
    return (await gpu.requestAdapter()) !== null;
  } catch {
    return false;
  }
}

function gpuFailed(e: unknown): null {
  console.warn("Drawing on the GPU failed, drawing in software:", e);
  return null;
}

/** VU1 in a worker of its own (vu1-worker.ts), where there are cores to
 * spare: it runs while the EE goes on. Needs SharedArrayBuffer (the page is
 * cross-origin isolated). */
function startVu1Worker(ps2emu: Ps2Emulator) {
  if (!globalThis.crossOriginIsolated || navigator.hardwareConcurrency < 4) return;
  const ops = Pipe.create(OPS_BYTES);
  const records = Pipe.create(RECORDS_BYTES);
  const worker = new Worker(new URL("./vu1-worker.ts", import.meta.url), { type: "module" });
  worker.postMessage({ ops, records, compiler: compiled !== null } satisfies ToVu1);
  const link = new Vu1Link(ops, records);
  ps2emu.setVu1Worker(
    (batch: Uint8Array) => link.send(batch),
    () => link.wait(),
  );
}

/** Netplay's hidden half, for the GPU's presentation. */
const HIDDEN_CODES = { top: 1, bottom: 2, left: 3, right: 4 } as const;

async function start(msg: StartMessage) {
  biosId = msg.biosId;
  const bios = await fetchBytes(`/api/bios/${encodeURIComponent(msg.biosId)}/data`);
  if (!bios) throw new Error("BIOS not found");
  if (msg.console === "ps2") {
    // Its own module, with the PlayStation's interface where they overlap.
    const ps2 = await import("./wasm/texel_web_ps2.js");
    await ps2.default();
    const onGpu: Ps2Emulator | null = (await webgpu()) ? await ps2.Ps2Emulator.withGpu(bios, msg.canvas).catch(gpuFailed) : null;
    gpu = onGpu;
    const ps2emu = onGpu ?? new ps2.Ps2Emulator(bios);
    // The EE's code compiled to WebAssembly as it runs; interpreted if this
    // browser will not make modules.
    try {
      ps2emu.setCompiler(true);
      ps2emu.setVuCompiler(true);
      compiled = "EE and VUs compiled";
    } catch (e) {
      console.warn("The EE's and VUs' compilers are not available, interpreting:", e);
    }
    startVu1Worker(ps2emu);
    emu = ps2emu as unknown as Emulator;
    gameId = null;
    if (msg.gameId) {
      // Its disc; its memory cards below, as the PlayStation's. (readFile
      // reads the game's.)
      gameId = msg.gameId;
      ps2Disc = true;
      ps2emu.insertDisc(
        msg.cue ?? undefined,
        msg.files.map((f) => f.name),
        Float64Array.from(msg.files.map((f) => f.size)),
        readFile,
      );
    }
  } else {
    ps1Memory = (await init()).memory;
    // Scaled drawing on the GPU, where the browser has WebGPU.
    const onGpu = (await webgpu()) ? await Emulator.withGpu(bios, msg.canvas).catch(gpuFailed) : null;
    gpu = onGpu;
    emu = onGpu ?? new Emulator(bios);
    // The CPU's code compiled to WebAssembly as it runs (it runs exactly as
    // interpreted: netplay peers may differ); interpreted if this browser
    // will not make modules.
    try {
      emu.setCompiler(true);
      compiled = "CPU compiled";
    } catch (e) {
      console.warn("The CPU's compiler is not available, interpreting:", e);
    }
    gameId = msg.gameId;
  }
  if (gameId && !ps2Disc) {
    emu.insertDisc(
      msg.cue ?? undefined,
      msg.files.map((f) => f.name),
      Float64Array.from(msg.files.map((f) => f.size)),
      readFile,
      (msg.sbi ? await fetchSbi(gameId) : undefined) ?? undefined,
    );
  }
  // Codes from the start (a netplay room's, for everyone the same).
  if (msg.codes) applyCodes(msg.codes);
  // A guest gets the host's memory cards with the game's state.
  const guest = msg.room !== null && msg.room.me !== msg.room.host;
  ownCards = !guest;
  cards = guest ? [null, null] : msg.cards;
  for (const slot of guest ? [] : MEMCARD_SLOTS) {
    const id = cards[slot];
    const card = id !== null && id !== undefined ? await fetchBytes(cardUrl(id)) : null;
    if (id !== null && id !== undefined && !card) throw new Error(`memory card ${slot + 1} not found`);
    emu.insertMemoryCard(slot, card ?? undefined);
  }
  canvas = msg.canvas;
  // Without the GPU, the PlayStation's scaled picture is drawn in another
  // worker where there are cores to spare; the canvas is then its own.
  if (!gpu && msg.console !== "ps2" && navigator.hardwareConcurrency >= 3) {
    drawer = new Worker(new URL("./draw-worker.ts", import.meta.url), { type: "module" });
    drawer.onmessage = (e: MessageEvent<FromDrawer>) => fromDrawer(e.data);
    drawer.postMessage({ type: "start", canvas } satisfies ToDrawer, [canvas]);
    emu.setDrawingElsewhere(true);
  } else if (!gpu) {
    // On the GPU, the canvas is WebGPU's: no 2D context.
    painter = makePainter(canvas);
  }
  smooth = msg.smooth;
  ring = msg.audio ? new Ring(msg.audio) : null;
  chosenScale = autoScale = msg.scale;
  emu.setResolutionScale(msg.scale);
  post({ type: "started", pal: emu.isPal, bios: emu.biosInfo(), renderer: renderer(), precise: preciseShown() });
  if (msg.room) pendingRoom = msg.room;
  if (pendingRoom) joinRoom(pendingRoom);
  pendingRoom = null;
  nextFrameAt = performance.now();
  statsAt = nextFrameAt;
  schedule(0);
}

function schedule(ms: number) {
  clearTimeout(timer);
  timer = setTimeout(tick, ms);
}

/** Fast-forward runs frames for this long, then lets messages in. */
const FAST_SLICE_MS = 15;

function frameInterval(): number {
  return 1000 / (emu?.isPal ? 49.76 : 59.83);
}

/** Applies cheat and patch codes (the PlayStation's: the PS2's module has
 * none): their settings first (CPU speed, RAM size), then the codes;
 * those refused are reported. */
function applyCodes(set: CodeSet | null) {
  const e = emu as {
    setCodes?: (codes: string) => string;
    setOverclock?: (percent: number) => void;
    setRamSize?: (mib: number) => void;
  } | null;
  if (!e?.setCodes) return;
  e.setOverclock?.(set?.overclock ?? 100);
  e.setRamSize?.(set?.ramMiB ?? 2);
  // A code a block: its activation and name, then its lines.
  const text = (set?.codes ?? []).map((c) => `${c.activation}\t${c.name}\n${c.body}`).join("\n\n");
  const refused = e
    .setCodes(text)
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [name = "", why = ""] = line.split("\t");
      return [name, why] as [string, string];
    });
  if (refused.length) post({ type: "codes-refused", refused });
}

/** Whether precise geometry can be shown: its triangles are recorded, for
 * the GPU or the drawing worker (the scaled picture drawn here, in
 * software, has no precise triangles). */
function preciseShown(): boolean {
  return gpu !== null || drawer !== null;
}

/** Runs frames as needed, paced by the audio queue when there is audio. */
function tick() {
  if (!emu || paused) return;
  try {
    const now = performance.now();
    let ran = 0;
    if (fast && !inStep()) {
      // As many frames as fit in a slice of time, then the page's turn.
      while (performance.now() - now < FAST_SLICE_MS && advance()) ran++;
    } else if (ring && audioRunning) {
      // Keep the audio queue filled; the audio clock paces emulation. A
      // frame a tick, each shown (the next tick is 4 ms away); several, of
      // which only the last is shown, only to catch up when the queue runs
      // low.
      const most = ring.available() < AUDIO_TARGET / 2 ? 4 : 1;
      while (ring.available() < AUDIO_TARGET && ran < most && advance()) ran++;
    } else {
      while (now >= nextFrameAt && ran < 4 && advance()) {
        nextFrameAt += frameInterval();
        ran++;
      }
      if (now - nextFrameAt > 250) nextFrameAt = now;
    }
    // Behind the room (the tab was hidden, or just joined): catch up.
    while (net && net.behind > 0 && ran < 12 && advance()) ran++;
    if (ran > 0) {
      draw();
      // (`frames` counts the second's frames, for the stats: a clock here.)
      if (now - cardsCheckedAt >= CARDS_EVERY_MS) {
        cardsCheckedAt = now;
        void saveMemoryCards();
      }
    }
    if (now - statsAt >= 1000) {
      post({ type: "stats", fps: (frames * 1000) / (now - statsAt), width: emu.frameWidth, height: emu.frameHeight });
      frames = 0;
      statsAt = now;
      adaptScale();
    }
  } catch (e) {
    post({ type: "error", message: String(e) });
    paused = true;
    return;
  }
  if (fast && !inStep()) schedule(0);
  else schedule(ring && audioRunning ? 4 : Math.max(0, nextFrameAt - performance.now()));
}

/** Runs the next frame, unless netplay waits for the other side's input. */
function advance(): boolean {
  if (net?.lockstep) return net.step();
  if (net?.waiting) return false;
  for (const port of [0, 1]) {
    const a = local.axes[port]!;
    emu?.setButtons(port, local.buttons[port]!);
    emu?.setAxes(port, a[0], a[1], a[2], a[3]);
  }
  runFrame();
  return true;
}

/** Last reported controller state, per port. */
const padState = ["", ""];

function reportPads() {
  if (!emu) return;
  for (const port of [0, 1]) {
    const analog = emu.isAnalog(port);
    const rumble = emu.rumble(port);
    const key = `${analog}:${rumble}`;
    if (key === padState[port]) continue;
    padState[port] = key;
    post({ type: "pad", port, analog, small: (rumble >> 8) / 255, large: (rumble & 0xff) / 255 });
  }
}

function runFrame() {
  if (!emu) return;
  const started = performance.now();
  emu.runFrame();
  emuMs = emuMs * 0.9 + (performance.now() - started) * 0.1;
  reportPads();
  frames++;
  const samples = emu.takeAudio();
  // Without audio playing, samples would pile up: drop them; so does
  // fast-forward (sound at several times its speed only crackles).
  if (ring && audioRunning && !fast && ring.available() + samples.length / 2 < RING_FRAMES) ring.push(samples);
}

/**
 * Draws the last frame. The canvas has the size it has on screen, in device
 * pixels, so the browser does no scaling of its own: the scaling mode is
 * ours (CSS `image-rendering` is not reliably applied to canvases drawn
 * from a worker).
 */
function draw() {
  if (gpu && canvas) {
    if (canvas.width !== viewport.width || canvas.height !== viewport.height) {
      canvas.width = viewport.width;
      canvas.height = viewport.height;
    }
    const hidden = net?.hiddenHalf;
    gpu.present(viewport.width, viewport.height, smooth, hidden ? HIDDEN_CODES[hidden] : 0);
    return;
  }
  if (!emu) return;
  const hidden = net?.hiddenHalf;
  if (drawer) {
    drawings.push((emu as Emulator).takeDrawing().buffer as ArrayBuffer);
    sendDrawings();
    return;
  }
  if (!painter) return;
  // The PlayStation's 15-bit picture, unconverted, where the painter takes
  // it (WebGL); else RGBA (the PS2's pictures, 24-bit ones, a 2D canvas).
  const layout = painter.takes15 && ps1Memory ? (emu as Emulator).picture() : undefined;
  if (layout && ps1Memory) painter.paint15(ps1Memory.buffer, layout, viewport, smooth, hidden);
  else painter.paint(emu.frame() as Uint8ClampedArray<ArrayBuffer>, emu.frameWidth, emu.frameHeight, viewport, smooth, hidden);
}

async function compress(data: Uint8Array, format: "gzip", decompress = false): Promise<Uint8Array> {
  const stream = new Blob([data as Uint8Array<ArrayBuffer>])
    .stream()
    .pipeThrough(decompress ? new DecompressionStream(format) : new CompressionStream(format));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

function stateUrl(slot: number, suffix = ""): string {
  return `/api/games/${encodeURIComponent(gameId ?? "")}/states/${slot}${suffix}`;
}

/** Sends the drawing worker what was recorded, when it is ready for it. */
function sendDrawings(screenshot = false) {
  if (!drawer || !drawerReady) return;
  drawerReady = false;
  const sent = drawings;
  drawings = [];
  const msg: ToDrawer = { type: "draw", drawings: sent, viewport, smooth, hidden: net?.hiddenHalf ?? null, screenshot };
  drawer.postMessage(msg, sent);
}

function fromDrawer(msg: FromDrawer) {
  if (msg.type === "error") {
    post({ type: "error", message: msg.message });
    return;
  }
  drawerReady = true;
  if (msg.ms > 0) drawMs = drawMs * 0.9 + msg.ms * 0.1;
  if (screenshotWanted) {
    screenshotWanted(msg.screenshot);
    screenshotWanted = null;
  }
  // What was recorded meanwhile goes at once.
  if (drawings.length > 0) sendDrawings();
}

/** The picture now, as PNG (for a save state's screenshot). */
async function screenshot(): Promise<Blob | null> {
  if (drawer) {
    const shot = new Promise<Blob | null>((resolve) => (screenshotWanted = resolve));
    // When the drawing worker is busy, the next round asks for it.
    if (drawerReady) sendDrawings(true);
    else drawings.push(new ArrayBuffer(0));
    return shot;
  }
  return painter ? painter.screenshot() : null;
}

/** Each second: lowers the resolution while emulation (or drawing) lags
 * behind the console's pace, and raises it back, up to the one chosen,
 * once there is clearly room for it. */
function adaptScale() {
  if (!emu || gpu || fast || paused) return;
  const budget = 1000 / (emu.isPal ? 50 : 60);
  const behind = emuMs > 0.9 * budget || drawMs > budget;
  behindFor = behind ? behindFor + 1 : 0;
  const now = performance.now();
  if (behindFor >= 2 && autoScale > 1 && now >= holdUntil) {
    setAutoScale(autoScale - 1);
    holdUntil = now + 5000;
    return;
  }
  // The next resolution's cost, guessed: drawing grows with its pixels;
  // emulation does not when drawing is elsewhere.
  const growth = ((autoScale + 1) / autoScale) ** 2;
  const next = drawer ? Math.max(emuMs, drawMs * growth) : emuMs * growth;
  roomFor = autoScale < chosenScale && next < 0.6 * budget ? roomFor + 1 : 0;
  if (roomFor >= 10 && now >= holdUntil) {
    setAutoScale(autoScale + 1);
    roomFor = 0;
    holdUntil = now + 15000;
  }
}

function setAutoScale(scale: number) {
  autoScale = scale;
  emu?.setResolutionScale(scale);
  emuMs = 0;
  drawMs = 0;
  post({ type: "auto-scale", scale, chosen: chosenScale });
}

async function saveState(slot: number) {
  if (!emu || !gameId) return;
  // Taken between frames, so the state and the screenshot match (the
  // PS2's is a promise, its GS memory read back from the GPU).
  const state = await emu.saveState();
  const shot = await screenshot();
  const body = await compress(state, "gzip");
  const resp = await fetch(`${stateUrl(slot)}?bios=${encodeURIComponent(biosId)}`, {
    method: "PUT",
    body: body as Uint8Array<ArrayBuffer>,
  });
  if (!resp.ok) throw new Error(`saving the state: HTTP ${resp.status} ${await resp.text()}`);
  if (shot) await fetch(stateUrl(slot, "/screenshot"), { method: "PUT", body: shot });
  post({ type: "state-saved", slot });
}

async function loadState(slot: number) {
  if (!emu || !gameId) return;
  const resp = await fetch(stateUrl(slot), { cache: "no-store" });
  if (!resp.ok) throw new Error(resp.status === 404 ? `slot ${slot} is empty` : `loading the state: HTTP ${resp.status}`);
  const state = await compress(new Uint8Array(await resp.arrayBuffer()), "gzip", true);
  emu.loadState(state);
  draw();
  post({ type: "state-loaded", slot });
}

/** Uploads, one after the other (an older one must not land last), and
 * cards whose upload failed, to send again. */
let uploads: Promise<void> = Promise.resolve();
const unsaved: (Uint8Array | null)[] = [null, null];

/** Sends the memory cards the game wrote since to the server; resolves
 * once every upload asked for so far is done. */
function saveMemoryCards(): Promise<void> {
  uploads = uploads.then(uploadCards);
  return uploads;
}

async function uploadCards() {
  // The cards' owner keeps the saves: a guest's cards are copies.
  if (!emu || !ownCards) return;
  for (const slot of MEMCARD_SLOTS) {
    const data = emu.takeWrittenMemoryCard(slot) ?? unsaved[slot];
    const id = cards[slot];
    // A blank card is not kept.
    if (!data || id === null || id === undefined) continue;
    let status: number | string;
    try {
      const resp = await fetch(cardUrl(id), {
        method: "PUT",
        body: data as Uint8Array<ArrayBuffer>,
      });
      status = resp.ok ? 0 : `HTTP ${resp.status}`;
    } catch (e) {
      status = String(e);
    }
    unsaved[slot] = status === 0 ? null : data;
    if (status === 0) post({ type: "saved", slot });
    else post({ type: "error", message: `saving memory card ${slot + 1}: ${status} (tried again soon)` });
  }
}

scope.onmessage = (e: MessageEvent<ToWorker>) => {
  const msg = e.data;
  switch (msg.type) {
    case "start":
      start(msg).catch((err: unknown) => post({ type: "error", message: String(err) }));
      break;
    case "buttons":
      local.buttons = msg.ports;
      if (net) net.input = { ...net.input, buttons: msg.ports[0] };
      break;
    case "axes":
      local.axes = msg.ports;
      if (net) net.input = { ...net.input, axes: msg.ports[0] };
      break;
    case "analog-button":
      if (net?.lockstep || net?.waiting) net.pressAnalog();
      else {
        emu?.pressAnalogButton(msg.port);
        reportPads();
      }
      break;
    case "net-attach":
      // Before the emulator is ready: once it is.
      if (emu) joinRoom(msg.room);
      else pendingRoom = msg.room;
      break;
    case "net-in":
      if (net) net.receive(msg.data);
      else netQueue.push(msg.data);
      break;
    case "net-ports":
      net?.setPorts(msg.ports);
      break;
    case "net-view":
      net?.setView(msg.view);
      break;
    case "net-leave":
      net?.close();
      break;
    case "audio":
      audioRunning = msg.running;
      // Switching to the timer: start counting from now.
      nextFrameAt = performance.now();
      break;
    case "codes":
      applyCodes(msg.set);
      break;
    case "precise":
      // The PlayStation's only (the PS2's module has none).
      (emu as { setPreciseGeometry?: (on: boolean) => void } | null)?.setPreciseGeometry?.(msg.on && preciseShown());
      break;
    case "widescreen":
      (emu as { setWidescreen?: (on: boolean) => void } | null)?.setWidescreen?.(msg.on);
      break;
    case "fast":
      fast = msg.on;
      // Back to real time from now.
      nextFrameAt = performance.now();
      schedule(0);
      break;
    case "pause":
      paused = msg.paused;
      if (paused) void saveMemoryCards();
      else schedule(0);
      break;
    case "stop":
      // The player goes: once the memory cards are saved.
      paused = true;
      net?.close();
      void saveMemoryCards().finally(() => post({ type: "stopped" }));
      break;
    case "reset":
      emu?.reset();
      break;
    case "save-state":
      saveState(msg.slot).catch((err: unknown) => post({ type: "error", message: String(err) }));
      break;
    case "load-state":
      loadState(msg.slot).catch((err: unknown) => post({ type: "error", message: String(err) }));
      break;
    case "swap":
      if (!emu) break;
      void saveMemoryCards();
      emu.openLid();
      // Games poll the lid: leave it open a moment, like a person would.
      void Promise.all([
        msg.sbi ? fetchSbi(msg.gameId) : Promise.resolve(null),
        new Promise((resolve) => setTimeout(resolve, 1500)),
      ]).then(([sbi]) => {
        if (!emu) return;
        gameId = msg.gameId;
        emu.insertDisc(
          msg.cue ?? undefined,
          msg.files.map((f) => f.name),
          Float64Array.from(msg.files.map((f) => f.size)),
          readFile,
          sbi ?? undefined,
        );
        post({ type: "swapped", gameId: msg.gameId });
      });
      break;
    case "viewport":
      viewport = { width: Math.max(1, msg.width), height: Math.max(1, msg.height) };
      // Redraw now: when paused, no frame would follow.
      draw();
      break;
    case "scale":
      chosenScale = autoScale = msg.scale;
      holdUntil = 0;
      emu?.setResolutionScale(msg.scale);
      post({ type: "auto-scale", scale: msg.scale, chosen: msg.scale });
      break;
    case "smooth":
      smooth = msg.smooth;
      draw();
      break;
  }
};
