// Gamepad layouts: the browser's "standard" one, or one set up by the user
// (saved per gamepad model) for pads the browser does not know.

import { Button } from "./buttons";

/** A control read from a gamepad button, or from an axis position. */
export type Binding =
  | { kind: "button"; index: number }
  /**
   * An axis reaching `target`, from `rest`. Triggers go from -1 to 1;
   * D-pads reported as a single "hat" axis use one value per direction
   * (and rest out of range), which must then match exactly.
   */
  | { kind: "axis"; index: number; target: number; rest: number };

export interface StickBinding {
  index: number;
  /** 1 if pushing right/down raises the axis, -1 if it lowers it. */
  sign: number;
}

export type ControlName =
  | "up" | "down" | "left" | "right"
  | "cross" | "circle" | "square" | "triangle"
  | "l1" | "r1" | "l2" | "r2"
  | "select" | "start" | "l3" | "r3" | "analog";
export type StickName = "lx" | "ly" | "rx" | "ry";

export interface Mapping {
  buttons: Partial<Record<ControlName, Binding>>;
  sticks: Partial<Record<StickName, StickBinding>>;
}

/** Controller buttons, with how they are asked for while setting up. */
export const CONTROLS: { name: ControlName; label: string; bit: number | null }[] = [
  { name: "up", label: "D-pad up", bit: Button.Up },
  { name: "down", label: "D-pad down", bit: Button.Down },
  { name: "left", label: "D-pad left", bit: Button.Left },
  { name: "right", label: "D-pad right", bit: Button.Right },
  { name: "cross", label: "✕ (Cross)", bit: Button.Cross },
  { name: "circle", label: "○ (Circle)", bit: Button.Circle },
  { name: "square", label: "□ (Square)", bit: Button.Square },
  { name: "triangle", label: "△ (Triangle)", bit: Button.Triangle },
  { name: "l1", label: "L1", bit: Button.L1 },
  { name: "r1", label: "R1", bit: Button.R1 },
  { name: "l2", label: "L2", bit: Button.L2 },
  { name: "r2", label: "R2", bit: Button.R2 },
  { name: "select", label: "Select", bit: Button.Select },
  { name: "start", label: "Start", bit: Button.Start },
  { name: "l3", label: "L3 (press the left stick in)", bit: Button.L3 },
  { name: "r3", label: "R3 (press the right stick in)", bit: Button.R3 },
  { name: "analog", label: "Analog (the PS or Home button)", bit: null },
];

export const STICKS: { name: StickName; label: string }[] = [
  { name: "lx", label: "Push the left stick to the right" },
  { name: "ly", label: "Push the left stick down" },
  { name: "rx", label: "Push the right stick to the right" },
  { name: "ry", label: "Push the right stick down" },
];

/** https://w3c.github.io/gamepad/#remapping */
export const STANDARD: Mapping = {
  buttons: Object.fromEntries(
    (
      [
        ["cross", 0], ["circle", 1], ["square", 2], ["triangle", 3],
        ["l1", 4], ["r1", 5], ["l2", 6], ["r2", 7],
        ["select", 8], ["start", 9], ["l3", 10], ["r3", 11],
        ["up", 12], ["down", 13], ["left", 14], ["right", 15], ["analog", 16],
      ] as const
    ).map(([name, index]) => [name, { kind: "button", index }]),
  ),
  sticks: { lx: { index: 0, sign: 1 }, ly: { index: 1, sign: 1 }, rx: { index: 2, sign: 1 }, ry: { index: 3, sign: 1 } },
};

const KEY = "texel.gamepad.";

export function savedMapping(id: string): Mapping | null {
  try {
    const raw = localStorage.getItem(KEY + id);
    return raw ? (JSON.parse(raw) as Mapping) : null;
  } catch {
    return null;
  }
}

export function saveMapping(id: string, mapping: Mapping | null) {
  try {
    if (mapping) localStorage.setItem(KEY + id, JSON.stringify(mapping));
    else localStorage.removeItem(KEY + id);
  } catch {
    // Private mode: kept for this page only.
  }
}

/** The layout to use for a pad: the user's, else the standard one when the
 * browser maps it, else none (then the standard one is a guess). */
export function mappingFor(pad: Gamepad): { mapping: Mapping; known: boolean; custom: boolean } {
  const saved = savedMapping(pad.id);
  if (saved) return { mapping: saved, known: true, custom: true };
  return { mapping: STANDARD, known: pad.mapping === "standard", custom: false };
}

/** "Sony PLAYSTATION(R)3 Controller (STANDARD GAMEPAD Vendor: 054c …)" or
 * "054c-0268-Sony PLAYSTATION(R)3 Controller" → "Sony PLAYSTATION(R)3 Controller". */
export function padName(pad: Gamepad): string {
  return pad.id.replace(/^[0-9a-f]{1,4}-[0-9a-f]{1,4}-/i, "").replace(/\s*\(.*?Vendor.*\)\s*$/i, "").trim() || "Gamepad";
}

function hatLike(b: { target: number; rest: number }): boolean {
  return Math.abs(b.target) < 0.95 || Math.abs(b.rest) > 1.05;
}

export function pressed(pad: Gamepad, b: Binding | undefined): boolean {
  if (!b) return false;
  if (b.kind === "button") {
    const button = pad.buttons[b.index];
    return !!button && (button.pressed || button.value > 0.5);
  }
  const v = pad.axes[b.index];
  if (v === undefined) return false;
  if (hatLike(b)) return Math.abs(v - b.target) < 0.1;
  return (v - b.rest) * Math.sign(b.target - b.rest) > Math.abs(b.target - b.rest) / 2;
}

export function stick(pad: Gamepad, s: StickBinding | undefined): number {
  if (!s) return 0;
  const v = pad.axes[s.index] ?? 0;
  return Math.max(-1, Math.min(1, v * s.sign));
}

/** A snapshot of a pad's buttons and axes. */
interface Snapshot {
  buttons: number[];
  axes: number[];
}

function snapshot(pad: Gamepad): Snapshot {
  return { buttons: pad.buttons.map((b) => (b.pressed ? 1 : b.value)), axes: [...pad.axes] };
}

/**
 * Learns a layout, one control at a time: each step waits until something
 * changes from the resting state, records it, then waits for release.
 */
export class Learner {
  readonly mapping: Mapping = { buttons: {}, sticks: {} };
  private step = 0;
  private rest: Snapshot | null = null;
  private waitingForRelease = false;
  /** Control and stick names, in order. */
  readonly steps: ({ kind: "button"; name: ControlName; label: string } | { kind: "stick"; name: StickName; label: string })[] = [
    ...CONTROLS.map((c) => ({ kind: "button" as const, name: c.name, label: c.label })),
    ...STICKS.map((s) => ({ kind: "stick" as const, name: s.name, label: s.label })),
  ];

  get index(): number {
    return this.step;
  }

  get done(): boolean {
    return this.step >= this.steps.length;
  }

  skip() {
    this.step++;
    this.waitingForRelease = false;
  }

  /** Call every frame. Returns true when the step changed. */
  update(pad: Gamepad): boolean {
    if (this.done) return false;
    const now = snapshot(pad);
    // The resting state is taken while nothing is held.
    if (!this.rest) {
      this.rest = now;
      return false;
    }
    const rest = this.rest;
    const moved = (i: number) => Math.abs((now.axes[i] ?? 0) - (rest.axes[i] ?? 0));
    const anyActive =
      now.buttons.some((v, i) => v > 0.5 && (rest.buttons[i] ?? 0) <= 0.5) || now.axes.some((_, i) => moved(i) > 0.3);
    if (this.waitingForRelease) {
      if (!anyActive) {
        this.waitingForRelease = false;
        this.step++;
        return true;
      }
      return false;
    }
    const current = this.steps[this.step]!;
    const button = current.kind === "button" ? now.buttons.findIndex((v, i) => v > 0.5 && (rest.buttons[i] ?? 0) <= 0.5) : -1;
    let axis = -1;
    now.axes.forEach((_, i) => {
      if (moved(i) > 0.5 && (axis < 0 || moved(i) > moved(axis))) axis = i;
    });
    if (current.kind === "button" && button >= 0) {
      this.mapping.buttons[current.name] = { kind: "button", index: button };
    } else if (axis >= 0) {
      const v = now.axes[axis]!;
      const r = rest.axes[axis]!;
      if (current.kind === "stick") {
        this.mapping.sticks[current.name] = { index: axis, sign: Math.sign(v - r) || 1 };
      } else {
        // Full deflection snaps to ±1; hat directions keep their exact value.
        const target = Math.abs(v) > 0.95 ? Math.sign(v) : Math.round(v * 1000) / 1000;
        this.mapping.buttons[current.name] = { kind: "axis", index: axis, target, rest: r };
      }
    } else {
      return false;
    }
    this.waitingForRelease = true;
    return false;
  }
}
