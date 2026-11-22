// Messages between the page and the emulator worker.

export interface StartMessage {
  type: "start";
  /** Which emulator: the PS2's is a module of its own, loaded on demand. */
  console: "ps1" | "ps2";
  biosId: string;
  gameId: string | null;
  /** The game whose memory cards to insert: the game's own by default
   * (without a game: blank cards, not kept). */
  cardsId: string | null;
  cue: string | null;
  files: { name: string; size: number }[];
  /** The disc has an SBI file (LibCrypt protection). */
  sbi: boolean;
  canvas: OffscreenCanvas;
  /** Audio ring buffer (see audio.ts), or null without audio. */
  audio: SharedArrayBuffer | null;
  smooth: boolean;
  scale: number;
  /** Netplay: the room to join, and this member's name. */
  join: { code: string; name: string } | null;
}

/** Owners of each controller: member ids (several share a controller). */
export type Ports = [number[], number[]];

export type Half = "top" | "bottom" | "left" | "right";
/** Split screen: the half controller 1 sees (controller 2 sees the other);
 * null: everyone sees the whole screen. */
export type SplitView = Half | null;

export interface Member {
  id: number;
  name: string;
  host: boolean;
  me: boolean;
  /** Has the game's state and runs it. */
  playing: boolean;
  /** Round trip to the server, in ms. */
  rtt: number | null;
}

export interface NetStatus {
  /** connecting → syncing (guests) → playing ⇄ stalled → ended. */
  state: "connecting" | "syncing" | "playing" | "stalled" | "ended";
  code: string | null;
  /** This member's id (0: the host). */
  me: number | null;
  members: Member[];
  ports: Ports;
  /** Round trip to the server, in ms. */
  rtt: number | null;
  /** This member's input delay, in frames. */
  delay: number | null;
  /** Members the game waits for, while stalled. */
  waitingFor: string[];
  /** Set by the host. */
  view: SplitView;
  /** The half hidden from this member, under the split view. */
  hidden: Half | null;
  note: string | undefined;
}

export interface ChatLine {
  /** null: a notice (someone joined, left...). */
  from: number | null;
  name: string;
  text: string;
  /** Unix ms. */
  at: number;
}

export type ToWorker =
  | StartMessage
  | { type: "buttons"; ports: [number, number] }
  /** Sticks of each port: left X, left Y, right X, right Y (0-255). */
  | { type: "axes"; ports: [Axes, Axes] }
  | { type: "analog-button"; port: number }
  | { type: "pause"; paused: boolean }
  /** PlayStation enhancements: precise geometry (GPU), widescreen. */
  | { type: "precise"; on: boolean }
  | { type: "widescreen"; on: boolean }
  /** Fast-forward: frames as fast as the machine runs them, without sound. */
  | { type: "fast"; on: boolean }
  /** Whether the audio output plays: frames are then paced by it. */
  | { type: "audio"; running: boolean }
  /** Size of the canvas on screen, in device pixels: the worker scales frames to it. */
  | { type: "viewport"; width: number; height: number }
  /** Smooth (bilinear) or sharp (nearest-neighbour) scaling. */
  | { type: "smooth"; smooth: boolean }
  /** Internal resolution multiplier, 1 to 4. */
  | { type: "scale"; scale: number }
  | { type: "reset" }
  | { type: "save-state"; slot: number }
  | { type: "load-state"; slot: number }
  /** Opens the lid, then closes it on another disc of the game. */
  | { type: "swap"; gameId: string; cue: string | null; files: { name: string; size: number }[]; sbi: boolean }
  /** Opens a netplay room for the running game. */
  | { type: "net-host"; gameId: string; biosId: string; name: string }
  /** Host: who drives each controller. */
  | { type: "net-ports"; ports: Ports }
  | { type: "net-chat"; text: string }
  /** Host: hides the other player's half of the screen from each player. */
  | { type: "net-view"; view: SplitView }
  | { type: "net-leave" };

export type Axes = [number, number, number, number];

export type FromWorker =
  /** Controller state, sent when it changes: analog mode and rumble (0-1). */
  | { type: "pad"; port: number; analog: boolean; small: number; large: number }
  /** renderer: what the PS2 runs on beyond software (the GS on WebGPU, the
   * EE's code compiled), or null. */
  | { type: "started"; pal: boolean; bios: string; renderer: string | null }
  | { type: "stats"; fps: number; width: number; height: number }
  /** The resolution drawn changed: lowered while emulation lags (`chosen`:
   * the one chosen, which it returns to). */
  | { type: "auto-scale"; scale: number; chosen: number }
  | { type: "saved"; slot: number }
  | { type: "swapped"; gameId: string }
  | { type: "state-saved"; slot: number }
  | { type: "state-loaded"; slot: number }
  | { type: "net"; status: NetStatus }
  | { type: "net-chat"; line: ChatLine }
  | { type: "error"; message: string };
