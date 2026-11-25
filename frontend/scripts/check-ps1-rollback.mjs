// Checks netplay's rollback (the core's rewind states, see netplay.ts): a
// game runs on two consoles with the same inputs, changing every few
// frames. One runs them as they come; the other learns each frame's input
// LATE frames afterwards, as a remote player's would arrive: it runs on
// the last input known, keeps a state before every frame, and when an
// input turns out otherwise goes back to that frame and runs the frames
// since again. Every 60 frames, the state before a frame whose inputs are
// all known must be the same on both. Prints what saving, loading and
// replaying cost.
//
//   pnpm run wasm
//   node scripts/check-ps1-rollback.mjs BIOS DISC.cue [FRAMES] [LATE] [interpreted]
import { fstatSync, openSync, readFileSync, readSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [biosPath, cuePath, framesArg, lateArg, mode] = process.argv.slice(2);
if (!biosPath || !cuePath) {
  console.error("usage: node scripts/check-ps1-rollback.mjs BIOS DISC.cue [FRAMES] [LATE] [interpreted]");
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

function make(compiled) {
  const emu = new mod.Emulator(bios);
  emu.insertDisc(cue, names, sizes, read, undefined);
  emu.setCompiler(compiled);
  return emu;
}
const FRAMES = Number(framesArg ?? 3000);
const LATE = Number(lateArg ?? 4);
const RING = 10;
// The reference: interpreted, when asked (the other is compiled).
const reference = make(mode !== "interpreted");
const rolling = make(true);

// Buttons held for 3 to 12 frames, Start and Cross often (to get past
// menus), the D-pad otherwise.
let seed = 12345;
const random = () => ((seed = (seed * 1103515245 + 12345) >>> 0) >>> 16) / 65536;
const inputs = [];
while (inputs.length < FRAMES) {
  const pick = random();
  const buttons = pick < 0.15 ? 1 << 3 : pick < 0.4 ? 1 << 14 : pick < 0.5 ? 0 : 1 << (4 + Math.floor(random() * 4));
  const hold = 3 + Math.floor(random() * 10);
  for (let k = 0; k < hold; k++) inputs.push(buttons);
}

const sums = new Map();
const used = [];
let saveMs = 0;
let loadMs = 0;
let replayed = 0;
let rollbacks = 0;
let compared = 0;
const time = (f) => {
  const t = performance.now();
  f();
  return performance.now() - t;
};
const runWith = (emu, buttons) => {
  emu.setButtons(0, buttons);
  emu.runFrame();
  emu.takeAudio();
};

for (let f = 0; f < FRAMES; f++) {
  // The reference, before frame f.
  reference.rewindSave(0);
  if (f % 60 === 0) sums.set(f, reference.rewindChecksum(0));
  runWith(reference, inputs[f]);

  // Frame f - LATE's input arrives: was it the one predicted?
  const g = f - LATE;
  if (g >= 0 && g < f && used[g] !== inputs[g]) {
    rollbacks++;
    loadMs += time(() => rolling.rewindLoad(g % RING));
    for (let k = g; k < f; k++) {
      if (k > g) saveMs += time(() => rolling.rewindSave(k % RING));
      used[k] = k <= g ? inputs[k] : inputs[g];
      runWith(rolling, used[k]);
      replayed++;
    }
  }
  saveMs += time(() => rolling.rewindSave(f % RING));
  // RESTORE=1: goes back to the state just kept, every frame (where
  // going back alone loses something).
  if (process.env.RESTORE) rolling.rewindLoad(f % RING);
  // The states before frames up to g are final now (LATE 0: no
  // prediction, a check of the comparison itself).
  if (g >= 0 && g % 60 === 0) {
    compared++;
    const mine = rolling.rewindChecksum(g % RING);
    if (mine !== sums.get(g)) {
      console.log(`DIFFERENT before frame ${g}: ${mine} rather than ${sums.get(g)} (${rollbacks} rollbacks so far)`);
      process.exit(1);
    }
  }
  used[f] = g >= 0 ? inputs[g] : 0;
  runWith(rolling, used[f]);
  // Compiled modules come in the background: let them in.
  if (f % 50 === 0) await new Promise((resolve) => setImmediate(resolve));
}
const saves = FRAMES + replayed - rollbacks;
console.log(
  `the same over ${FRAMES} frames (${compared} compared), input ${LATE} frames late: ${rollbacks} rollbacks, ` +
    `${replayed} frames replayed; a save ${(saveMs / saves).toFixed(3)} ms, a load ${(loadMs / Math.max(1, rollbacks)).toFixed(3)} ms`,
);
