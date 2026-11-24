// VU1's worker, in Node, for check-ps2-vu1-worker.mjs: as src/vu1-worker.ts.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { workerData } from "node:worker_threads";
import { fail, serve } from "../src/vu1-pipe.ts";

const { ops, records, compiler } = workerData;
try {
  const dir = fileURLToPath(new URL("../src/wasm/", import.meta.url));
  const mod = await import(`${dir}texel_web_ps2.js`);
  await mod.default({ module_or_path: readFileSync(`${dir}texel_web_ps2_bg.wasm`) });
  const vu1 = new mod.Vu1Worker();
  if (compiler) vu1.setCompiler(true);
  serve(ops, records, (batch) => vu1.process(batch));
} catch (err) {
  fail(records);
  throw err;
}
