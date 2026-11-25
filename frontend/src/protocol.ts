// Messages between the page and the emulator worker.

export interface StartMessage {
  type: "start";
  /** Which emulator: the PS2's is a module of its own, loaded on demand. */
  console: "ps1" | "ps2";
  biosId: string;
  gameId: string | null;
  /** The user's memory cards to insert, in slots 1 and 2 (null: a blank
   * card, not kept). */
  cards: Slots;
  cue: string | null;
  files: { name: string; size: number }[];
  /** The disc has an SBI file (LibCrypt protection). */
  sbi: boolean;
  canvas: OffscreenCanvas;
  /** Audio ring buffer (see audio.ts), or null without audio. */
  audio: SharedArrayBuffer | null;
  smooth: boolean;
  scale: number;
  /** Netplay: the room whose game this is, to run in step with it. */
  room: RoomSnapshot | null;
  /** Cheat and patch codes, from the start. */
  codes: CodeSet | null;
}

/** A code enabled (see the core's `cheats`): its lines with its `?` digits
 * filled. */
export interface EnabledCode {
  name: string;
  body: string;
  activation: string;
}

/** The cheat and patch codes a game runs with, and what they need of the
 * emulator (the largest any asks). A netplay room shares its host's. */
export interface CodeSet {
  codes: EnabledCode[];
  /** The CPU's speed, in percent (100: the console's). */
  overclock: number;
  /** RAM, in MiB (2, or 8). */
  ramMiB: number;
  /** The picture shown 16:9 (a patch renders it so). */
  wide: boolean;
  /** No widescreen by the GTE (a patch does its own). */
  noGteWidescreen: boolean;
}

/** A memory card per slot: the user's card ids (null: none of theirs). */
export type Slots = [number | null, number | null];

/** Owners of each controller: member ids (several share a controller). */
export type Ports = [number[], number[]];

export type Half = "top" | "bottom" | "left" | "right";
/** Split screen: the half controller 1 sees (controller 2 sees the other);
 * null: everyone sees the whole screen. */
export type SplitView = Half | null;

/** What a player needs of the room to run its game in step: who is
 * there, who hosts, the epoch, the controllers and split screen. */
export interface RoomSnapshot {
  me: number;
  host: number;
  epoch: number;
  members: { id: number; name: string; registered: boolean; rtt: number | null }[];
  ports: Ports;
  /** The host handed out the controllers (no automatic second one). */
  assigned: boolean;
  view: SplitView;
  /** Rollback rather than lockstep (the room's, from the server). */
  rollback: boolean;
}

export interface Member {
  id: number;
  name: string;
  host: boolean;
  /** Signed in: may take over as the host. */
  registered: boolean;
  me: boolean;
  /** Runs the game in step. */
  playing: boolean;
  /** Round trip to the server, in ms. */
  rtt: number | null;
}

/** A room's game, as the player runs it. */
export interface NetStatus {
  /** syncing (guests) → playing ⇄ stalled → ended. */
  state: "syncing" | "playing" | "stalled" | "ended";
  epoch: number;
  me: number;
  members: Member[];
  ports: Ports;
  assigned: boolean;
  /** This member's input delay, in frames. */
  delay: number | null;
  /** Rollback rooms: frames run ahead on predicted inputs now, and the
   * frames replayed in the last few seconds (corrections). */
  rollback: { ahead: number; replayed: number } | null;
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
  /** The player goes: the worker saves the memory cards, then `stopped`. */
  | { type: "stop" }
  /** PlayStation enhancements: precise geometry (GPU), widescreen. */
  | { type: "precise"; on: boolean }
  /** The codes enabled from now on (see `CodeSet`). */
  | { type: "codes"; set: CodeSet }
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
  /** The running game becomes the room's (the host's, as it is). */
  | { type: "net-attach"; room: RoomSnapshot }
  /** A message from the room. */
  | { type: "net-in"; data: string | ArrayBuffer }
  /** Host: who drives each controller. */
  | { type: "net-ports"; ports: Ports }
  /** Split screen (the page tells the others). */
  | { type: "net-view"; view: SplitView }
  | { type: "net-leave" };

export type Axes = [number, number, number, number];


export type FromWorker =
  /** Controller state, sent when it changes: analog mode and rumble (0-1). */
  | { type: "pad"; port: number; analog: boolean; small: number; large: number }
  /** renderer: what the PS2 runs on beyond software (the GS on WebGPU, the
   * EE's code compiled), or null. */
  /** precise: precise geometry can be shown (the PlayStation's, drawn on
   * the GPU or in the drawing worker). */
  | { type: "started"; pal: boolean; bios: string; renderer: string | null; precise: boolean }
  /** Codes the core refused, and why. */
  | { type: "codes-refused"; refused: [string, string][] }
  | { type: "stats"; fps: number; width: number; height: number }
  /** The resolution drawn changed: lowered while emulation lags (`chosen`:
   * the one chosen, which it returns to). */
  | { type: "auto-scale"; scale: number; chosen: number }
  | { type: "saved"; slot: number }
  | { type: "swapped"; gameId: string }
  | { type: "state-saved"; slot: number }
  | { type: "state-loaded"; slot: number }
  | { type: "net"; status: NetStatus }
  /** A message for the room. */
  | { type: "net-out"; data: string | Uint8Array }
  | { type: "stopped" }
  | { type: "error"; message: string };
