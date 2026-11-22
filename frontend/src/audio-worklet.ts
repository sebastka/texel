// Audio worklet: plays the ring buffer filled by the emulator worker.

import { Ring } from "./audio";

// Minimal declarations of the AudioWorkletGlobalScope.
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(name: string, ctor: new (options: { processorOptions: unknown }) => unknown): void;

class RingPlayer extends AudioWorkletProcessor {
  private readonly ring: Ring;

  constructor(options: { processorOptions: unknown }) {
    super();
    this.ring = new Ring(options.processorOptions as SharedArrayBuffer);
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const [left, right] = outputs[0] ?? [];
    if (left && right) this.ring.pull(left, right);
    return true;
  }
}

registerProcessor("ring-player", RingPlayer);
