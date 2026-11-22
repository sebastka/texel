// Keyboard and gamepad input: controller button masks (1 = pressed) and
// analog sticks.

import { Button } from "./buttons";
import { mappingFor, pressed, stick, CONTROLS } from "./gamepad";
import type { Axes } from "./protocol";

export { Button };

/**
 * Keyboard layout, by `KeyboardEvent.code`. Split like the controller: the
 * left hand has the D-pad (WASD, or the arrow keys) and L1/L2, the right
 * hand the face buttons in their diamond (I J K L) and R1/R2.
 */
export const KEYS: Record<string, number> = {
  KeyW: Button.Up,
  KeyA: Button.Left,
  KeyS: Button.Down,
  KeyD: Button.Right,
  ArrowUp: Button.Up,
  ArrowDown: Button.Down,
  ArrowLeft: Button.Left,
  ArrowRight: Button.Right,
  KeyI: Button.Triangle,
  KeyJ: Button.Square,
  KeyK: Button.Cross,
  KeyL: Button.Circle,
  KeyQ: Button.L1,
  KeyE: Button.R1,
  Digit1: Button.L2,
  Digit3: Button.R2,
  Enter: Button.Start,
  ShiftRight: Button.Select,
  Backspace: Button.Select,
};

export class Input {
  private keys = 0;
  private readonly onKey = (e: KeyboardEvent) => {
    const bit = KEYS[e.code];
    if (bit === undefined || e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
    e.preventDefault();
    this.keys = e.type === "keydown" ? this.keys | bit : this.keys & ~bit;
  };
  private readonly onBlur = () => {
    this.keys = 0;
  };

  constructor() {
    window.addEventListener("keydown", this.onKey);
    window.addEventListener("keyup", this.onKey);
    window.addEventListener("blur", this.onBlur);
  }

  dispose() {
    window.removeEventListener("keydown", this.onKey);
    window.removeEventListener("keyup", this.onKey);
    window.removeEventListener("blur", this.onBlur);
  }

  /** Gamepads, in port order. */
  gamepads(): Gamepad[] {
    // Missing on insecure origins (plain http, but for localhost).
    return (navigator.getGamepads?.() ?? []).filter((p): p is Gamepad => p !== null && p.connected).slice(0, 2);
  }

  /**
   * Buttons and sticks of each port: keyboard and first gamepad on 1,
   * second gamepad on 2. In digital mode, the left stick also drives the
   * d-pad, as there is nothing else to read it.
   */
  poll(analog: [boolean, boolean]): { buttons: [number, number]; axes: [Axes, Axes]; home: [boolean, boolean] } {
    const buttons: [number, number] = [this.keys, 0];
    const axes: [Axes, Axes] = [
      [128, 128, 128, 128],
      [128, 128, 128, 128],
    ];
    const home: [boolean, boolean] = [false, false];
    this.gamepads().forEach((pad, i) => {
      // The pad's layout: set up by the user, else the browser's standard one.
      const { mapping } = mappingFor(pad);
      let mask = 0;
      for (const c of CONTROLS) {
        if (c.bit !== null && pressed(pad, mapping.buttons[c.name])) mask |= c.bit;
      }
      home[i] = pressed(pad, mapping.buttons.analog);
      const [lx, ly, rx, ry] = (["lx", "ly", "rx", "ry"] as const).map((n) => stick(pad, mapping.sticks[n])) as [
        number,
        number,
        number,
        number,
      ];
      const byte = (v: number) => Math.max(0, Math.min(255, Math.round((v + 1) * 127.5)));
      axes[i] = [byte(lx), byte(ly), byte(rx), byte(ry)];
      if (!analog[i]) {
        if (lx < -0.5) mask |= Button.Left;
        if (lx > 0.5) mask |= Button.Right;
        if (ly < -0.5) mask |= Button.Up;
        if (ly > 0.5) mask |= Button.Down;
      }
      buttons[i] = (buttons[i] ?? 0) | mask;
    });
    return { buttons, axes, home };
  }

  /** Drives a gamepad's motors (0-1 each), when it has any. */
  rumble(port: number, small: number, large: number) {
    const actuator = this.gamepads()[port]?.vibrationActuator;
    if (!actuator) return;
    if (small === 0 && large === 0) void actuator.reset().catch(() => undefined);
    else
      void actuator
        .playEffect("dual-rumble", { duration: 1000, strongMagnitude: large, weakMagnitude: small })
        .catch(() => undefined);
  }
}
