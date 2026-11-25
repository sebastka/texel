// Checks the PlayStation's CPU compiler: runs a game in the WebAssembly
// module twice, with the CPU's code compiled and interpreted, and compares
// the two consoles' state (a checksum of the whole machine) every second
// of console time: they must be the same. Then prints their speeds and
// what the compiler did. Exits with 1 if they differ.
//
//   pnpm run wasm
//   node scripts/check-ps1-compiler.mjs BIOS DISC.cue [SECONDS]
//
// PRESS="600:start,800:cross": hold a button for 6 frames from a frame.
// COMPILED_ONLY=1 runs only the compiled console (to measure it).
// OVERCLOCK=200 (the CPU's speed, %), RAM=8 (MiB), CODES=file (cheat and
// patch codes, as `setCodes` takes them): both consoles with them.
import { fstatSync, openSync, readFileSync, readSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [biosPath, cuePath, secondsArg] = process.argv.slice(2);
if (!biosPath || !cuePath) {
  console.error("usage: node scripts/check-ps1-compiler.mjs BIOS DISC.cue [SECONDS]");
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
// EVERY=1: compare every frame (finds the first frame they differ).
const every = Number(process.env.EVERY ?? 50);

async function run(compiled) {
  const emu = new mod.Emulator(bios);
  emu.insertDisc(cue, names, sizes, read, undefined);
  emu.setCompiler(compiled);
  if (process.env.OVERCLOCK) emu.setOverclock(Number(process.env.OVERCLOCK));
  if (process.env.RAM) emu.setRamSize(Number(process.env.RAM));
  if (process.env.CODES) {
    const refused = emu.setCodes(readFileSync(process.env.CODES, "utf8"));
    if (refused) console.log(`codes refused:\n${refused}`);
  }
  const sums = [];
  const started = performance.now();
  for (let f = 1; f <= frames; f++) {
    const held = presses.filter(([at]) => f >= at && f < at + 6).reduce((a, [, b]) => a | b, 0);
    emu.setButtons(0, held);
    emu.runFrame();
    emu.takeAudio();
    if (f % every === 0) sums.push(emu.stateChecksum());
    // Compiled modules come in the background: let them in.
    if (f % 10 === 0) await new Promise((resolve) => setImmediate(resolve));
  }
  const fps = (frames * 1000) / (performance.now() - started);
  console.log(`${compiled ? "compiled" : "interpreted"}: ${fps.toFixed(1)} frames per second; ${emu.compilerStats()}`);
  return sums;
}

const compiled = await run(true);
if (!process.env.COMPILED_ONLY) {
  const interpreted = await run(false);
  let same = true;
  interpreted.forEach((h, i) => {
    const ok = h === compiled[i];
    const first = !ok && same;
    same &&= ok;
    if (first || i % 10 === 9) console.log(`${((i + 1) * every) / 50} s: ${ok ? "same" : `DIFFERENT (interpreted ${h}, compiled ${compiled[i]})`}`);
  });
  console.log(same ? "all the same" : "DIFFERENT");
  process.exit(same ? 0 : 1);
}
