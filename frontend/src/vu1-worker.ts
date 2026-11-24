// VU1, in a worker of its own (another core): the emulation worker hands it
// the core's queued VU1 work through vu1-pipe.ts, and waits for what VU1 sent
// the GIF at the core's syncs. VU1 runs while the EE goes on.

import init, { Vu1Worker } from "./wasm/texel_web_ps2.js";
import { fail, serve } from "./vu1-pipe";

export interface ToVu1 {
  ops: SharedArrayBuffer;
  records: SharedArrayBuffer;
  /** VU1's microprograms compiled to WebAssembly, as the emulation worker's. */
  compiler: boolean;
}

const scope = self as unknown as DedicatedWorkerGlobalScope;

scope.onmessage = (e: MessageEvent<ToVu1>) => {
  const { ops, records, compiler } = e.data;
  init()
    .then(() => {
      const vu1 = new Vu1Worker();
      if (compiler) vu1.setCompiler(true);
      // Blocks this worker for good: it waits on the pipe between batches.
      serve(ops, records, (batch) => vu1.process(batch));
    })
    .catch((err: unknown) => {
      fail(records);
      console.error("VU1's worker failed:", err);
    });
};
