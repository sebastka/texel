// Checks and measures the PS2's VU1 compiler on a game: runs the same
// stretch of a game twice, VU1's microprograms interpreted and compiled,
// compares the two consoles' state every second of console time (they must
// be the same), and prints their speeds (frames and VU1 instructions per
// second). Exits with 1 if they differ. The EE's code is compiled in both.
//
//   pnpm run wasm
//   node scripts/check-ps2-vu.mjs PS2-BIOS DISC.iso START.state [SECONDS]
//
// The first run boots the game and plays it to START (PRESS, as the boot
// example takes it: "18:cross,22:cross", seconds of console time; AT: the
// second to save at, 40 by default), and saves the state there; the next
// runs start from it. The GS draws nothing (GFX=1: in software).
// INTERPRETED_ONLY=1 or COMPILED_ONLY=1: one console only.
import { closeSync, existsSync, fstatSync, openSync, readFileSync, readSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const [biosPath, discPath, statePath, secondsArg] = process.argv.slice(2);
if (!statePath) {
  console.error("usage: node scripts/check-ps2-vu.mjs PS2-BIOS DISC.iso START.state [SECONDS]");
  process.exit(2);
}
const dir = fileURLToPath(new URL("../src/wasm/", import.meta.url));
const mod = await import(`${dir}texel_web_ps2.js`);
await mod.default({ module_or_path: readFileSync(`${dir}texel_web_ps2_bg.wasm`) });
const bios = readFileSync(biosPath);
const seconds = Number(secondsArg ?? 10);
const BUTTONS = { cross: 1 << 14, circle: 1 << 13, square: 1 << 15, triangle: 1 << 12, start: 1 << 3, up: 1 << 4, down: 1 << 6, left: 1 << 7, right: 1 << 5 };

/** A console with the disc in, from the start state (made on the first run). */
async function console_(vuCompiled) {
  const emu = new mod.Ps2Emulator(bios);
  if (!process.env.GFX) emu.disableGraphics();
  emu.setCompiler(true);
  if (vuCompiled !== null) {
    if (!emu.setVuCompiler) throw new Error("this build has no VU compiler");
    emu.setVuCompiler(vuCompiled);
  }
  const fd = openSync(discPath, "r");
  const size = fstatSync(fd).size;
  emu.insertDisc(undefined, ["disc.iso"], Float64Array.from([size]), (_file, offset, length) => {
    const buf = new Uint8Array(length);
    readSync(fd, buf, 0, length, offset);
    return buf;
  });
  if (existsSync(statePath)) {
    emu.loadState(readFileSync(statePath));
    return emu;
  }
  // The first run: boot and play to the start.
  const fps = emu.isPal ? 50 : 60;
  const presses = (process.env.PRESS ?? "18:cross,22:cross")
    .split(",")
    .filter(Boolean)
    .map((p) => {
      const [t, b] = p.split(":");
      return [Math.round(Number(t) * fps), BUTTONS[b]];
    });
  const end = Math.round(Number(process.env.AT ?? 40) * fps);
  const started = performance.now();
  for (let f = 0; f < end; f++) {
    const held = presses.filter(([at]) => f >= at && f < at + Math.round(fps / 5)).reduce((m, [, b]) => m | b, 0);
    emu.setButtons(0, held);
    emu.runFrame();
  }
  emu.setButtons(0, 0);
  writeFileSync(statePath, await emu.saveState());
  console.log(`made ${statePath} (${(end / fps).toFixed(0)} s of console time in ${((performance.now() - started) / 1000).toFixed(0)} s)`);
  closeSync(fd);
  return console_(vuCompiled);
}

async function run(vuCompiled) {
  const emu = await console_(vuCompiled);
  const fps = emu.isPal ? 50 : 60;
  const hashes = [];
  const steps0 = emu.vu1Steps();
  const started = performance.now();
  for (let f = 1; f <= seconds * fps; f++) {
    emu.runFrame();
    if (f % fps === 0) hashes.push(emu.stateHash());
  }
  const secs = (performance.now() - started) / 1000;
  const steps = emu.vu1Steps() - steps0;
  const name = vuCompiled === null ? "VU1 interpreted (no compiler in this build)" : vuCompiled ? "VU1 compiled" : "VU1 interpreted";
  const stats = vuCompiled && emu.vuCompilerStats ? `; ${emu.vuCompilerStats()}` : "";
  console.log(
    `${name}: ${((seconds * fps) / secs).toFixed(1)} frames per second, ` +
      `${(steps / secs / 1e6).toFixed(1)} M VU1 instructions per second (${(steps / seconds / 1e6).toFixed(1)} M per console second)${stats}`,
  );
  return hashes;
}

const hasCompiler = typeof mod.Ps2Emulator.prototype.setVuCompiler === "function";
if (!hasCompiler) {
  await run(null);
  process.exit(0);
}
const interpreted = process.env.COMPILED_ONLY ? null : await run(false);
const compiled = process.env.INTERPRETED_ONLY ? null : await run(true);
if (interpreted && compiled) {
  let same = true;
  interpreted.forEach((h, i) => {
    const ok = h === compiled[i];
    same &&= ok;
    console.log(`${i + 1} s: ${ok ? "same" : `DIFFERENT (interpreted ${h}, compiled ${compiled[i]})`}`);
  });
  process.exit(same ? 0 : 1);
}
