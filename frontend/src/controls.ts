// The keyboard mapping, drawn: a PS1-style digital controller, with a
// leader line from each button to its key. Built from KEYS, so it always
// shows the actual bindings.

import { Button, KEYS } from "./input";

const NS = "http://www.w3.org/2000/svg";

function el<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number>, ...children: (Node | string)[]) {
  const e = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  for (const c of children) e.append(c);
  return e;
}

/** `KeyboardEvent.code` → what is printed on the key. */
function keyName(code: string): string {
  const arrows: Record<string, string> = { ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→" };
  if (code in arrows) return arrows[code]!;
  if (code.startsWith("Key")) return code.slice(3);
  if (code.startsWith("Digit")) return code.slice(5);
  if (code === "ShiftRight") return "Right ⇧";
  if (code === "ShiftLeft") return "Left ⇧";
  return code;
}

function keysFor(...buttons: number[]): string[] {
  return Object.entries(KEYS)
    .filter(([, b]) => buttons.includes(b))
    .map(([code]) => keyName(code));
}

interface Label {
  keys: string[];
  caption: string;
  /** Where the keycaps are (row centre), and the line's path to the button. */
  x: number;
  y: number;
  side: "left" | "right" | "below";
  path: [number, number][];
  /** Direction keys laid out as on a keyboard: up above left, down, right. */
  cluster?: [string, string, string, string];
}

/** Keycaps, one per key, laid out from (x, y) in the direction of `side`. */
function keycaps(label: Label): { group: SVGGElement; width: number } {
  const group = el("g", {});
  const cap = (x: number, y: number, w: number, k: string) =>
    group.append(
      el("rect", { class: "keycap", x, y: y - 12, width: w, height: 24, rx: 5 }),
      el("text", { class: "keycap-text", x: x + w / 2, y: y + 4.5, "text-anchor": "middle" }, k),
    );
  if (label.cluster) {
    // Right-aligned on label.x: left, down, right on the bottom row, up above down.
    const [up, left, down, right] = label.cluster;
    const [w, gap] = [26, 4];
    const x0 = label.x - (3 * w + 2 * gap);
    cap(x0 + w + gap, label.y - 13, w, up);
    [left, down, right].forEach((k, i) => cap(x0 + i * (w + gap), label.y + 13, w, k));
    return { group, width: 3 * w + 2 * gap };
  }
  const widths = label.keys.map((k) => Math.max(26, k.length * 7.5 + 14));
  const gap = 4;
  const total = widths.reduce((a, w) => a + w, 0) + gap * (widths.length - 1);
  let x = label.side === "left" ? label.x - total : label.side === "below" ? label.x - total / 2 : label.x;
  label.keys.forEach((k, i) => {
    const w = widths[i]!;
    cap(x, label.y, w, k);
    x += w + gap;
  });
  return { group, width: total };
}

/** A direction cluster, if these four keys are bound to the D-pad. */
function cluster(codes: [string, string, string, string]): [string, string, string, string] | null {
  const dirs = [Button.Up, Button.Left, Button.Down, Button.Right];
  return codes.every((c, i) => KEYS[c] === dirs[i]) ? (codes.map(keyName) as [string, string, string, string]) : null;
}

export function controlsDiagram(): SVGSVGElement {
  const svg = el("svg", { viewBox: "0 0 720 322", class: "controls-diagram", role: "img" });
  const summary = (
    [
      ["D-pad", keysFor(Button.Up, Button.Down, Button.Left, Button.Right)],
      ["Cross", keysFor(Button.Cross)],
      ["Circle", keysFor(Button.Circle)],
      ["Square", keysFor(Button.Square)],
      ["Triangle", keysFor(Button.Triangle)],
      ["L1", keysFor(Button.L1)],
      ["L2", keysFor(Button.L2)],
      ["R1", keysFor(Button.R1)],
      ["R2", keysFor(Button.R2)],
      ["Start", keysFor(Button.Start)],
      ["Select", keysFor(Button.Select)],
    ] as const
  )
    .map(([b, k]) => `${b}: ${k.join(" or ")}`)
    .join(", ");
  svg.append(el("title", {}, `Keyboard controls. ${summary}.`));

  // Shoulder buttons, behind the body: L2/R2 further back.
  for (const [x, label] of [
    [210, "L2"],
    [440, "R2"],
  ] as const) {
    svg.append(el("rect", { class: "pad-shoulder back", x, y: 48, width: 70, height: 20, rx: 6 }));
    svg.append(el("text", { class: "pad-legend", x: x + 35, y: 61, "text-anchor": "middle" }, label));
  }
  for (const [x, label] of [
    [205, "L1"],
    [435, "R1"],
  ] as const) {
    svg.append(el("rect", { class: "pad-shoulder", x, y: 64, width: 80, height: 22, rx: 7 }));
    svg.append(el("text", { class: "pad-legend", x: x + 40, y: 79, "text-anchor": "middle" }, label));
  }

  // Body, with its two grips.
  svg.append(
    el("path", {
      class: "pad-body",
      d:
        "M 250 80 H 470 C 530 80 552 112 557 152 C 566 205 560 252 522 258 C 488 262 470 236 456 206 " +
        "C 442 178 410 170 360 170 C 310 170 278 178 264 206 C 250 236 232 262 198 258 " +
        "C 160 252 154 205 163 152 C 168 112 190 80 250 80 Z",
    }),
  );

  // D-pad: a cross, with an arrow on each arm.
  const [dx, dy] = [245, 130];
  svg.append(
    el("path", {
      class: "pad-button",
      d: `M ${dx - 7} ${dy - 22} h 14 v 15 h 15 v 14 h -15 v 15 h -14 v -15 h -15 v -14 h 15 Z`,
    }),
  );
  for (const [ax, ay, rot] of [
    [dx, dy - 15, 0],
    [dx + 15, dy, 90],
    [dx, dy + 15, 180],
    [dx - 15, dy, 270],
  ] as const) {
    svg.append(el("path", { class: "pad-arrow", d: "M 0 -3 L 3.5 2 H -3.5 Z", transform: `translate(${ax} ${ay}) rotate(${rot})` }));
  }

  // Face buttons, with their usual symbol colours.
  const [fx, fy] = [475, 130];
  const face = (x: number, y: number, cls: string, symbol: SVGElement) => {
    svg.append(el("circle", { class: "pad-button", cx: x, cy: y, r: 11 }));
    symbol.setAttribute("class", `pad-symbol ${cls}`);
    svg.append(symbol);
  };
  face(fx, fy - 25, "triangle", el("path", { d: `M ${fx} ${fy - 31} L ${fx + 6} ${fy - 21} H ${fx - 6} Z` }));
  face(fx + 25, fy, "circle", el("circle", { cx: fx + 25, cy: fy, r: 5.5 }));
  face(fx, fy + 25, "cross", el("path", { d: `M ${fx - 5} ${fy + 20} L ${fx + 5} ${fy + 30} M ${fx + 5} ${fy + 20} L ${fx - 5} ${fy + 30}` }));
  face(fx - 25, fy, "square", el("rect", { x: fx - 30, y: fy - 5, width: 10, height: 10 }));

  // Select and Start.
  for (const [x, label] of [
    [335, "SELECT"],
    [385, "START"],
  ] as const) {
    svg.append(el("rect", { class: "pad-button", x: x - 11, y: 127, width: 22, height: 9, rx: 4.5 }));
    svg.append(el("text", { class: "pad-legend small", x, y: 149, "text-anchor": "middle" }, label));
  }

  const labels: Label[] = [
    { keys: keysFor(Button.L2), caption: "L2", x: 150, y: 28, side: "left", path: [[150, 28], [190, 28], [212, 52]] },
    { keys: keysFor(Button.L1), caption: "L1", x: 150, y: 64, side: "left", path: [[150, 64], [180, 64], [205, 75]] },
    ...[
      { codes: ["KeyW", "KeyA", "KeyS", "KeyD"] as [string, string, string, string], y: 122, path: [[150, 122], [223, 128]] as [number, number][] },
      {
        codes: ["ArrowUp", "ArrowLeft", "ArrowDown", "ArrowRight"] as [string, string, string, string],
        y: 188,
        path: [[150, 188], [190, 188], [223, 133]] as [number, number][],
      },
    ].flatMap(({ codes, y, path }) => {
      const keys = cluster(codes);
      return keys ? [{ keys, cluster: keys, caption: "D-pad", x: 150, y, side: "left" as const, path }] : [];
    }),
    { keys: keysFor(Button.R2), caption: "R2", x: 590, y: 40, side: "right", path: [[590, 40], [530, 40], [508, 52]] },
    { keys: keysFor(Button.R1), caption: "R1", x: 590, y: 80, side: "right", path: [[590, 80], [515, 77]] },
    { keys: keysFor(Button.Triangle), caption: "Triangle", x: 590, y: 110, side: "right", path: [[590, 110], [520, 110], [486, 105]] },
    { keys: keysFor(Button.Circle), caption: "Circle", x: 590, y: 140, side: "right", path: [[590, 140], [530, 140], [511, 132]] },
    { keys: keysFor(Button.Cross), caption: "Cross", x: 590, y: 175, side: "right", path: [[590, 175], [520, 175], [484, 162]] },
    { keys: keysFor(Button.Square), caption: "Square", x: 590, y: 215, side: "right", path: [[590, 215], [450, 215], [450, 141]] },
    { keys: keysFor(Button.Select), caption: "Select", x: 290, y: 288, side: "below", path: [[290, 276], [290, 236], [335, 160], [335, 136]] },
    { keys: keysFor(Button.Start), caption: "Start", x: 430, y: 288, side: "below", path: [[430, 276], [430, 236], [385, 160], [385, 136]] },
  ];
  for (const label of labels) {
    if (!label.keys.length) continue;
    const points = label.path.map(([x, y]) => `${x},${y}`).join(" ");
    const [ex, ey] = label.path[label.path.length - 1]!;
    svg.append(el("polyline", { class: "leader", points }), el("circle", { class: "leader-dot", cx: ex, cy: ey, r: 2.5 }));
    const { group, width } = keycaps(label);
    svg.append(group);
    const caption =
      label.side === "left"
        ? el("text", { class: "caption", x: label.x - width - 8, y: label.y + 4, "text-anchor": "end" }, label.caption)
        : label.side === "right"
          ? el("text", { class: "caption", x: label.x + width + 8, y: label.y + 4 }, label.caption)
          : el("text", { class: "caption", x: label.x, y: label.y + 22, "text-anchor": "middle" }, label.caption);
    svg.append(caption);
  }
  return svg;
}
