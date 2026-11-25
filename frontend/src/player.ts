// The player: owns the emulator worker, the canvas, audio and input.

import type { Console, GameDetail } from "./api";
import { createRing } from "./audio";
import workletUrl from "./audio-worklet.ts?worker&url";
import { Input } from "./input";
import type { CodeSet, FromWorker, NetStatus, Ports, RoomSnapshot, Slots, SplitView, StartMessage, ToWorker } from "./protocol";

interface Callbacks {
  onStatus: (s: string) => void;
  /** A save state slot was written. */
  onStateSaved?: (slot: number) => void;
  onPause: (paused: boolean) => void;
  /** The first controller's analog mode changed (by the user or the game). */
  onAnalog: (analog: boolean) => void;
  /** Running: with what renderer (WebGPU, or null for software). */
  /** precise: precise geometry can be shown. */
  onStarted?: (renderer: string | null, precise: boolean) => void;
  /** Codes the emulator refused, and why. */
  onCodesRefused?: (refused: [string, string][]) => void;
  /** The room's game, as this player runs it. */
  onNet?: (status: NetStatus) => void;
  /** A message for the room. */
  /** A message for the room; `datagram`: for its datagram session. */
  onNetOut?: (data: string | Uint8Array, datagram: boolean) => void;
}

const SMOOTH_KEY = "texel.smooth";
const PRECISE_KEY = "texel.precise";
const WIDESCREEN_KEY = "texel.widescreen";

/** A remembered on/off setting (`fallback` when none, or no storage). */
function loadFlag(key: string, fallback: boolean): boolean {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : v === "1";
  } catch {
    return fallback;
  }
}

function saveFlag(key: string, on: boolean) {
  try {
    localStorage.setItem(key, on ? "1" : "0");
  } catch {
    // Private mode: not remembered.
  }
}
/** How long a stopped worker may take to save memory cards. */
const STOP_TIMEOUT_MS = 10_000;
/** Largest canvas side, in device pixels (a 4K screen is 3840 wide). */
const MAX_CANVAS = 4096;
const SCALE_KEY = "texel.scale";

function loadScale(): number {
  try {
    const v = Number(localStorage.getItem(SCALE_KEY));
    return v >= 1 && v <= 4 ? v : 1;
  } catch {
    return 1;
  }
}

function loadSmooth(): boolean {
  try {
    return localStorage.getItem(SMOOTH_KEY) === "1";
  } catch {
    return false;
  }
}

export class Player {
  private readonly worker: Worker;
  private readonly input = new Input();
  private audio: AudioContext | null = null;
  private raf = 0;
  private lastButtons = "";
  /** Game input is suspended while a gamepad is being set up. */
  inputSuspended = false;
  private lastAxes = "";
  private homeHeld: [boolean, boolean] = [false, false];
  private analog: [boolean, boolean] = [false, false];
  /** Rumble, re-sent periodically: gamepad effects have a duration. */
  private rumble: [[number, number], [number, number]] = [
    [0, 0],
    [0, 0],
  ];
  private rumbleAt = 0;
  private paused = false;
  /** Fast-forward is on. */
  fast = false;
  private pal = false;
  /** The GPU drawing the PS2's GS, or null in software. */
  private renderer: string | null = null;
  /** Shown in the status while the resolution is lowered automatically. */
  private autoNote = "";
  private resize: ResizeObserver | null = null;
  /** Smooth (bilinear) scaling, rather than sharp pixels. */
  smooth = loadSmooth();
  /** PlayStation enhancements, off unless chosen: precise geometry (it
   * only changes the picture), widescreen (it changes the game). */
  precise = loadFlag(PRECISE_KEY, false);
  widescreen = loadFlag(WIDESCREEN_KEY, false);
  /** Internal resolution multiplier. */
  scale = loadScale();

  constructor(
    private readonly screen: HTMLElement,
    private readonly cb: Callbacks,
  ) {
    this.worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
    this.worker.onmessage = (e: MessageEvent<FromWorker>) => this.onMessage(e.data);
    this.worker.onerror = (e) => cb.onStatus(`Error: ${e.message}`);
    document.addEventListener("visibilitychange", this.onVisibility);
  }

  private send(msg: ToWorker, transfer: Transferable[] = []) {
    this.worker.postMessage(msg, transfer);
  }

  /** Starts a game with the user's memory `cards`; with `room`, as that
   * netplay room's game (a guest then gets the host's cards). */
  async start(biosId: string, game: GameDetail, cards: Slots, room: RoomSnapshot | null = null, codes: CodeSet | null = null) {
    await this.launch({
      console: "ps1",
      biosId,
      gameId: game.id,
      cards,
      cue: game.cue,
      files: game.files,
      sbi: game.sbi,
      room,
      codes,
    });
  }

  /** The cheat and patch codes enabled from now on. */
  setCodes(set: CodeSet | null) {
    this.send({ type: "codes", set: set ?? { codes: [], overclock: 100, ramMiB: 2, wide: false, noGteWidescreen: false } });
  }

  /** A console: its BIOS, to its menu, or for the PS2 with a game's disc
   * (booted directly); with the user's memory `cards`. */
  async startConsole(platform: Console, biosId: string, cards: Slots, game?: GameDetail) {
    await this.launch({
      console: platform,
      biosId,
      gameId: game?.id ?? null,
      cards,
      cue: game?.cue ?? null,
      files: game?.files ?? [],
      sbi: false,
      room: null,
      codes: null,
    });
  }

  private async launch(what: Pick<StartMessage, "console" | "biosId" | "gameId" | "cards" | "cue" | "files" | "sbi" | "room" | "codes">) {
    const canvas = document.createElement("canvas");
    canvas.width = 640;
    canvas.height = 480;
    this.screen.replaceChildren(canvas);
    const offscreen = canvas.transferControlToOffscreen();
    // Keep the canvas at its on-screen size in device pixels.
    this.resize = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (!entry) return;
      const device = entry.devicePixelContentBoxSize?.[0];
      const box = entry.contentBoxSize[0];
      // Capped: a canvas too large is not drawn at all.
      const width = Math.min(MAX_CANVAS, Math.round(device?.inlineSize ?? (box?.inlineSize ?? 640) * devicePixelRatio));
      const height = Math.min(MAX_CANVAS, Math.round(device?.blockSize ?? (box?.blockSize ?? 480) * devicePixelRatio));
      this.send({ type: "viewport", width, height });
    });
    this.resize.observe(canvas);
    let ring: SharedArrayBuffer | null = null;
    if (crossOriginIsolated) {
      try {
        ring = createRing();
        // The PlayStation's SPU makes 44.1 kHz, the PS2's SPU2 48 kHz.
        this.audio = new AudioContext({ sampleRate: what.console === "ps2" ? 48_000 : 44_100, latencyHint: "interactive" });
        await this.audio.audioWorklet.addModule(workletUrl);
        const node = new AudioWorkletNode(this.audio, "ring-player", {
          outputChannelCount: [2],
          processorOptions: ring,
        });
        node.connect(this.audio.destination);
        // Browsers start audio only once the page was clicked (or a key
        // pressed): do not wait for it, as a page opened from a link may
        // never be. Until then, the worker paces frames with a timer.
        const audio = this.audio;
        const report = () => this.send({ type: "audio", running: audio.state === "running" });
        audio.onstatechange = report;
        report();
        void audio.resume().catch(() => undefined);
        if (audio.state !== "running") {
          const unlock = () => {
            if (this.paused || audio.state === "closed") return;
            void audio.resume().then(() => {
              if (audio.state === "running") {
                window.removeEventListener("pointerdown", unlock, true);
                window.removeEventListener("keydown", unlock, true);
              }
            });
          };
          window.addEventListener("pointerdown", unlock, true);
          window.addEventListener("keydown", unlock, true);
        }
      } catch (e) {
        console.warn("audio disabled", e);
        ring = null;
      }
    } else {
      console.warn("not cross-origin isolated: audio disabled");
    }
    this.send(
      {
        type: "start",
        ...what,
        canvas: offscreen,
        audio: ring,
        smooth: this.smooth,
        scale: this.scale,
      },
      [offscreen],
    );
    this.pollInput();
  }

  private onMessage(msg: FromWorker) {
    switch (msg.type) {
      case "codes-refused":
        this.cb.onCodesRefused?.(msg.refused);
        break;
      case "started":
        this.pal = msg.pal;
        this.renderer = msg.renderer;
        this.send({ type: "precise", on: this.precise });
        this.send({ type: "widescreen", on: this.widescreen });
        this.cb.onStarted?.(msg.renderer, msg.precise);
        this.cb.onStatus("Running");
        break;
      case "stats":
        if (!this.paused) {
          const gpu = this.renderer ? ` · ${this.renderer}` : "";
          this.cb.onStatus(`${msg.fps.toFixed(0)} fps · ${msg.width}×${msg.height}${this.autoNote}${this.pal ? " · PAL" : ""}${gpu}`);
        }
        break;
      case "auto-scale":
        this.autoNote = msg.scale < msg.chosen ? ` (${msg.scale}× while catching up)` : "";
        break;
      case "pad":
        this.analog[msg.port] = msg.analog;
        this.rumble[msg.port] = [msg.small, msg.large];
        this.input.rumble(msg.port, msg.small, msg.large);
        if (msg.port === 0) this.cb.onAnalog(msg.analog);
        break;
      case "state-saved":
        this.cb.onStatus(`Saved to slot ${msg.slot}`);
        this.cb.onStateSaved?.(msg.slot);
        break;
      case "state-loaded":
        this.cb.onStatus(`Loaded slot ${msg.slot}`);
        break;
      case "swapped":
        this.cb.onStatus("Disc changed");
        break;
      case "saved":
        this.cb.onStatus(`Memory card ${msg.slot + 1} saved`);
        break;
      case "net":
        this.cb.onNet?.(msg.status);
        break;
      case "net-out":
        this.cb.onNetOut?.(msg.data, msg.datagram === true);
        break;
      case "error":
        this.cb.onStatus(`Error: ${msg.message}`);
        break;
    }
  }

  private pollInput = () => {
    const polled = this.input.poll(this.analog);
    const { buttons, axes, home } = this.inputSuspended
      ? {
          buttons: [0, 0] as [number, number],
          axes: [
            [128, 128, 128, 128],
            [128, 128, 128, 128],
          ] as typeof polled.axes,
          home: [false, false] as [boolean, boolean],
        }
      : polled;
    const key = buttons.join(",");
    if (key !== this.lastButtons) {
      this.lastButtons = key;
      this.send({ type: "buttons", ports: buttons });
    }
    const axesKey = axes.join(";");
    if (axesKey !== this.lastAxes) {
      this.lastAxes = axesKey;
      this.send({ type: "axes", ports: axes });
    }
    // The gamepad's Home button is the Analog button.
    home.forEach((pressed, port) => {
      if (pressed && !this.homeHeld[port]) this.pressAnalog(port);
      this.homeHeld[port] = pressed;
    });
    const now = performance.now();
    if (now - this.rumbleAt > 800) {
      this.rumbleAt = now;
      this.rumble.forEach(([small, large], port) => {
        if (small || large) this.input.rumble(port, small, large);
      });
    }
    this.raf = requestAnimationFrame(this.pollInput);
  };

  /** The controller's Analog button. */
  pressAnalog(port = 0) {
    this.send({ type: "analog-button", port });
  }

  private onVisibility = () => {
    // Do not run in the background; the user resumes explicitly.
    if (document.hidden && !this.paused) this.togglePause();
  };

  /** Fast-forward: on or off; returns the new state. */
  toggleFast(): boolean {
    this.fast = !this.fast;
    this.send({ type: "fast", on: this.fast });
    return this.fast;
  }

  /** Precise geometry on or off; returns the new state. */
  togglePrecise(): boolean {
    this.precise = !this.precise;
    saveFlag(PRECISE_KEY, this.precise);
    this.send({ type: "precise", on: this.precise });
    return this.precise;
  }

  /** Widescreen on or off (`on`: to set it); returns the new state. */
  setWidescreen(on = !this.widescreen): boolean {
    this.widescreen = on;
    saveFlag(WIDESCREEN_KEY, on);
    this.send({ type: "widescreen", on });
    return on;
  }

  togglePause() {
    this.paused = !this.paused;
    this.send({ type: "pause", paused: this.paused });
    if (this.paused) void this.audio?.suspend();
    else void this.audio?.resume();
    this.cb.onPause(this.paused);
    this.cb.onStatus(this.paused ? "Paused" : "Running");
  }

  /** The running game becomes the room's: the host's, as it is. */
  attachNetplay(room: RoomSnapshot) {
    this.send({ type: "net-attach", room });
  }

  /** A message from the room. */
  netIn(data: string | ArrayBuffer) {
    this.send({ type: "net-in", data }, typeof data === "string" ? [] : [data]);
  }

  /** Host: who drives each controller. */
  setNetPorts(ports: Ports) {
    this.send({ type: "net-ports", ports });
  }

  /** Split screen (see SplitView). */
  setNetView(view: SplitView) {
    this.send({ type: "net-view", view });
  }

  leaveNetplay() {
    this.send({ type: "net-leave" });
  }

  reset() {
    this.send({ type: "reset" });
  }

  saveState(slot: number) {
    this.cb.onStatus(`Saving to slot ${slot}…`);
    this.send({ type: "save-state", slot });
  }

  loadState(slot: number) {
    this.cb.onStatus(`Loading slot ${slot}…`);
    this.send({ type: "load-state", slot });
  }

  /** Changes discs (multi-disc games). */
  swapDisc(game: GameDetail) {
    this.cb.onStatus("Changing disc…");
    this.send({ type: "swap", gameId: game.id, cue: game.cue, files: game.files, sbi: game.sbi });
  }

  /** Cycles the internal resolution: 1×, 2×, 3×, 4×. */
  cycleScale(): number {
    this.scale = (this.scale % 4) + 1;
    try {
      localStorage.setItem(SCALE_KEY, String(this.scale));
    } catch {
      // Private mode: not remembered.
    }
    this.send({ type: "scale", scale: this.scale });
    return this.scale;
  }

  toggleSmooth(): boolean {
    this.smooth = !this.smooth;
    try {
      localStorage.setItem(SMOOTH_KEY, this.smooth ? "1" : "0");
    } catch {
      // Private mode: not remembered.
    }
    this.send({ type: "smooth", smooth: this.smooth });
    return this.smooth;
  }

  stop() {
    // The worker stops, saves the memory cards the game wrote, and says
    // so: it ends then (or after a while, should the server not answer).
    this.send({ type: "stop" });
    cancelAnimationFrame(this.raf);
    this.rumble.forEach((_, port) => this.input.rumble(port, 0, 0));
    this.resize?.disconnect();
    this.input.dispose();
    document.removeEventListener("visibilitychange", this.onVisibility);
    void this.audio?.close();
    const end = () => this.worker.terminate();
    const timeout = setTimeout(end, STOP_TIMEOUT_MS);
    this.worker.onmessage = (e: MessageEvent<FromWorker>) => {
      if (e.data.type !== "stopped") return;
      clearTimeout(timeout);
      end();
    };
  }
}
