// Checks the PS2's EE compiler: boots a PS2 BIOS in the WebAssembly module
// twice, with the EE's code compiled and interpreted, and compares the two
// consoles' state every second of console time (they must be the same), then
// prints their speeds and what the compiler did. Exits with 1 if they differ.
//
//   pnpm run wasm
//   node scripts/check-ps2-compiler.mjs PS2-BIOS [SECONDS]
//
// The GS draws nothing (it would take most of the time); GFX=1 draws in
// software. COMPILED_ONLY=1 runs only the compiled console (to measure it).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const [biosPath, secondsArg] = process.argv.slice(2);
if (!biosPath) {
  console.error("usage: node scripts/check-ps2-compiler.mjs PS2-BIOS [SECONDS]");
  process.exit(2);
}
const dir = fileURLToPath(new URL("../src/wasm/", import.meta.url));
const mod = await import(`${dir}texel_web_ps2.js`);
await mod.default({ module_or_path: readFileSync(`${dir}texel_web_ps2_bg.wasm`) });
const bios = readFileSync(biosPath);
const frames = 60 * Number(secondsArg ?? 10);

function run(compiled) {
  const emu = new mod.Ps2Emulator(bios);
  if (!process.env.GFX) emu.disableGraphics();
  emu.setCompiler(compiled);
  const hashes = [];
  const started = performance.now();
  for (let f = 1; f <= frames; f++) {
    emu.runFrame();
    if (f % 60 === 0) hashes.push(emu.stateHash());
  }
  const fps = (frames * 1000) / (performance.now() - started);
  console.log(`${compiled ? "compiled" : "interpreted"}: ${fps.toFixed(1)} frames per second; ${emu.compilerStats()}`);
  return hashes;
}

const compiled = run(true);
if (!process.env.COMPILED_ONLY) {
  const interpreted = run(false);
  let same = true;
  interpreted.forEach((h, i) => {
    const ok = h === compiled[i];
    same &&= ok;
    console.log(`${i + 1} s: ${ok ? "same" : `DIFFERENT (interpreted ${h}, compiled ${compiled[i]})`}`);
  });
  process.exit(same ? 0 : 1);
}
