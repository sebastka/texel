// Checks deferred drawing (`Emulator.setDrawingDeferred`): runs a game in
// the WebAssembly module twice, both recording their drawing for a
// Replayer, as with a drawing worker; one draws the native resolution
// itself, the other leaves it to its replayer (fetching VRAM back from it
// when read). Every second of console time, compares the two consoles'
// state (a checksum of the whole machine, VRAM included) and the pictures
// their replayers show: they must be the same. Then prints the time
// emulation took per frame, each way. Exits with 1 if they differ.
//
//   pnpm run wasm
//   node scripts/check-ps1-deferred.mjs BIOS DISC.cue [SECONDS]
//
// PRESS="600:start,800:cross": hold a button for 6 frames from a frame.
// SCALE=2: the resolution the replayers draw at (1 by default).
// PRECISE=1: precise geometry (the scaled pictures use it).
import { fstatSync, openSync, readFileSync, readSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [biosPath, cuePath, secondsArg] = process.argv.slice(2);
if (!biosPath || !cuePath) {
  console.error("usage: node scripts/check-ps1-deferred.mjs BIOS DISC.cue [SECONDS]");
  process.exit(2);
}
const dir = fileURLToPath(new URL("../src/wasm/", import.meta.url));
const mod = await import(`${dir}texel_web.js`);
await mod.default({ module_or_path: readFileSync(`${dir}texel_web_bg.wasm`) });
const bios = readFileSync(biosPath);
const cue = readFileSync(cuePath, "utf8");
const names = [...cue.matchAll(/FILE "([^"]+)"/g)].map((m) => m[1]);
const fds = names.map((n) => openSync(join(dirname(cuePath), n)));
const sizes = Float64Array.from(fds.map((fd) => fstatSync(fd).size));
const read = (file, offset, length) => {
  const buf = new Uint8Array(length);
  readSync(fds[file], buf, 0, length, offset);
  return buf;
};
const BUTTONS = { start: 1 << 3, up: 1 << 4, right: 1 << 5, down: 1 << 6, left: 1 << 7, triangle: 1 << 12, circle: 1 << 13, cross: 1 << 14, square: 1 << 15 };
const presses = (process.env.PRESS ?? "")
  .split(",")
  .filter(Boolean)
  .map((p) => {
    const [f, b] = p.split(":");
    return [Number(f), BUTTONS[b] ?? 0];
  });
const frames = 50 * Number(secondsArg ?? 20);
const every = 50;

/** A console recording its drawing for its own replayer; `deferred`: the
 * native resolution drawn there only. */
function console_(deferred) {
  const emu = new mod.Emulator(bios);
  emu.insertDisc(cue, names, sizes, read, undefined);
  emu.setCompiler(true);
  emu.setDrawingElsewhere(true);
  emu.setResolutionScale(Number(process.env.SCALE ?? 1));
  emu.setPreciseGeometry(process.env.PRECISE === "1");
  const replayer = new mod.Replayer();
  let fetches = 0;
  if (deferred) {
    emu.setDrawingDeferred((commands) => {
      fetches++;
      replayer.applyCommands(commands);
      return replayer.nativeVram();
    });
  }
  return { emu, replayer, ms: 0, fetches: () => fetches };
}

const consoles = [console_(false), console_(true)];
let same = true;
for (let f = 1; f <= frames; f++) {
  const held = presses.filter(([at]) => f >= at && f < at + 6).reduce((a, [, b]) => a | b, 0);
  for (const c of consoles) {
    c.emu.setButtons(0, held);
    const started = performance.now();
    c.emu.runFrame();
    c.ms += performance.now() - started;
    c.emu.takeAudio();
    c.replayer.apply(c.emu.takeDrawing());
  }
  if (f % every === 0) {
    const [a, b] = consoles;
    const state = a.emu.stateChecksum() === b.emu.stateChecksum();
    a.replayer.render();
    b.replayer.render();
    const pa = a.replayer.frame();
    const pb = b.replayer.frame();
    const picture = pa.length === pb.length && pa.every((v, i) => v === pb[i]);
    const ok = state && picture;
    const first = !ok && same;
    same &&= ok;
    if (first || (f / every) % 10 === 0) {
      const what = ok ? "same" : `DIFFERENT (${state ? "" : "state "}${picture ? "" : "picture"})`;
      console.log(`${f / every} s: ${what}`);
    }
  }
  // Compiled modules come in the background: let them in.
  if (f % 10 === 0) await new Promise((resolve) => setImmediate(resolve));
}
const [a, b] = consoles;
console.log(`drawn here: ${(a.ms / frames).toFixed(2)} ms a frame; deferred: ${(b.ms / frames).toFixed(2)} (${b.fetches()} VRAM fetches)`);
console.log(same ? "all the same" : "DIFFERENT");
process.exit(same ? 0 : 1);
