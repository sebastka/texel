// Checks VU1 in a worker of its own (src/vu1-worker.ts): runs a game from a
// save state twice in the WebAssembly module, VU1 run by the emulation worker
// and by another, and compares the consoles' state every second of console
// time (they must be the same), then prints their speeds. Exits with 1 if
// they differ.
//
//   pnpm run wasm
//   node scripts/check-ps2-vu1-worker.mjs PS2-BIOS SECONDS DISC.iso STATE
//
// The EE's and VUs' code is compiled. The GS draws nothing (it would take
// most of the time); GFX=1 draws in software. WORKER_ONLY=1 runs only the
// console with VU1's worker (to measure it).
import { fstatSync, openSync, readFileSync, readSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Worker } from "node:worker_threads";
import { OPS_BYTES, Pipe, RECORDS_BYTES, Vu1Link } from "../src/vu1-pipe.ts";

const [biosPath, secondsArg, discPath, statePath] = process.argv.slice(2);
if (!biosPath || !statePath) {
  console.error("usage: node scripts/check-ps2-vu1-worker.mjs PS2-BIOS SECONDS DISC.iso STATE");
  process.exit(2);
}
const dir = fileURLToPath(new URL("../src/wasm/", import.meta.url));
const mod = await import(`${dir}texel_web_ps2.js`);
await mod.default({ module_or_path: readFileSync(`${dir}texel_web_ps2_bg.wasm`) });
const bios = readFileSync(biosPath);
const frames = 60 * Number(secondsArg);

async function run(inWorker) {
  const emu = new mod.Ps2Emulator(bios);
  if (!process.env.GFX) emu.disableGraphics();
  emu.setCompiler(true);
  emu.setVuCompiler(true);
  const fd = openSync(discPath, "r");
  emu.insertDisc(undefined, ["disc.iso"], Float64Array.from([fstatSync(fd).size]), (_file, offset, length) => {
    const buf = new Uint8Array(length);
    readSync(fd, buf, 0, length, offset);
    return buf;
  });
  emu.loadState(readFileSync(statePath));
  let worker = null;
  let link = null;
  if (inWorker) {
    const ops = Pipe.create(OPS_BYTES);
    const records = Pipe.create(RECORDS_BYTES);
    worker = new Worker(new URL("./vu1-node-worker.mjs", import.meta.url), {
      workerData: { ops, records, compiler: true },
    });
    link = new Vu1Link(ops, records);
    emu.setVu1Worker(
      (batch) => link.send(batch),
      () => link.wait(),
    );
  }
  const hashes = [];
  const started = performance.now();
  // The second half's speed too: most programs are compiled by then.
  let half = started;
  for (let f = 1; f <= frames; f++) {
    if (f === Math.floor(frames / 2) + 1) half = performance.now();
    emu.runFrame();
    if (f % 60 === 0) hashes.push(emu.stateHash());
    // The EE's compiled modules come in the background: let them in.
    await new Promise((resolve) => setImmediate(resolve));
  }
  const now = performance.now();
  const fps = (frames * 1000) / (now - started);
  const warm = ((frames - Math.floor(frames / 2)) * 1000) / (now - half);
  console.log(
    `${inWorker ? "VU1 in a worker" : "VU1 inline"}: ${fps.toFixed(1)} frames per second (second half ${warm.toFixed(1)}); ${emu.vuCompilerStats()}`,
  );
  if (link) {
    const s = (ms) => `${(ms / 1000).toFixed(1)} s`;
    console.log(
      `  in ${s(now - started)}: the EE side waited ${s(link.waitMs)} at syncs, sent for ${s(link.sendMs)}; VU1's worker ran batches for ${s(link.busyMs())}`,
    );
  }
  await worker?.terminate();
  return hashes;
}

const remote = await run(true);
if (!process.env.WORKER_ONLY) {
  const inline = await run(false);
  let same = true;
  inline.forEach((h, i) => {
    const ok = h === remote[i];
    same &&= ok;
    console.log(`${i + 1} s: ${ok ? "same" : `DIFFERENT (inline ${h}, worker ${remote[i]})`}`);
  });
  process.exit(same ? 0 : 1);
}
