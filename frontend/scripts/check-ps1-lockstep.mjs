// Finds where the PlayStation's CPU compiler and interpreter part: runs a
// game on two consoles in step, the CPU's code compiled on one and
// interpreted on the other: the compiled one a block at a time, the
// interpreted one an instruction at a time up to the same cycle. Where
// their cycle counts meet, their CPUs must be in the same state (the whole
// machine too, compared every 2000 meetings). Stops at the first
// difference, printing the last blocks run and the fields that differ.
// Slow (about a million cycles a second): for after check-ps1-compiler
// found a difference (with EVERY=1, its first frame).
//
//   pnpm run wasm
//   node scripts/check-ps1-lockstep.mjs BIOS DISC.cue [CYCLES]
import { fstatSync, openSync, readFileSync, readSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [biosPath, cuePath, cyclesArg] = process.argv.slice(2);
if (!biosPath || !cuePath) {
  console.error("usage: node scripts/check-ps1-lockstep.mjs BIOS DISC.cue [CYCLES]");
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
const compiled = make(true);
const interpreted = make(false);
const max = Number(cyclesArg ?? 40e6);
const pc = (emu) => emu.debugCpu().split(" ")[1];
const fields = (emu) => emu.debugCpuFields().split("\n");

let ca = 0;
let cb = 0;
let steps = 0;
let met = 0;
const recent = [];
function differs(what) {
  console.log(`DIFFERENT (${what}) at cycle ${ca}, after ${steps} compiled steps, ${met} meetings`);
  console.log(`last compiled steps (pc before -> after @cycles):\n  ${recent.join("\n  ")}`);
  const fb = fields(interpreted);
  fields(compiled).forEach((line, i) => {
    if (line !== fb[i]) console.log(`  compiled:    ${line}\n  interpreted: ${fb[i]}`);
  });
  process.exit(1);
}
while (ca < max) {
  const before = pc(compiled);
  ca = compiled.debugStep();
  recent.push(`${before} -> ${pc(compiled)} @${ca}`);
  if (recent.length > 12) recent.shift();
  while (cb < ca) cb = interpreted.debugStep();
  steps++;
  // Compiled modules come in the background: let them in.
  if (steps % 5000 === 0) await new Promise((resolve) => setImmediate(resolve));
  if (cb !== ca) continue;
  met++;
  if (compiled.debugCpu() !== interpreted.debugCpu()) differs("CPU");
  if (met % 2000 === 0 && compiled.stateChecksum() !== interpreted.stateChecksum()) differs("machine");
}
console.log(`the same up to cycle ${ca} (${steps} compiled steps, ${met} meetings)`);
