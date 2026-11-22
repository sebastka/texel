// A lock-free single-producer/single-consumer ring of stereo float samples
// in a SharedArrayBuffer: the worker writes, the audio worklet reads.

/** Stereo frames in the ring (~0.37 s at 44.1 kHz). */
export const RING_FRAMES = 16384;
const HEADER_BYTES = 8; // read and write indices (Int32), in frames

export function createRing(): SharedArrayBuffer {
  return new SharedArrayBuffer(HEADER_BYTES + RING_FRAMES * 2 * 4);
}

export class Ring {
  private readonly idx: Int32Array;
  private readonly data: Float32Array;

  constructor(buffer: SharedArrayBuffer) {
    this.idx = new Int32Array(buffer, 0, 2);
    this.data = new Float32Array(buffer, HEADER_BYTES, RING_FRAMES * 2);
  }

  /** Frames waiting to be played. */
  available(): number {
    const read = Atomics.load(this.idx, 0);
    const write = Atomics.load(this.idx, 1);
    return (write - read + RING_FRAMES) % RING_FRAMES;
  }

  /** Writes interleaved i16 stereo samples; drops what does not fit. */
  push(samples: Int16Array): void {
    let write = Atomics.load(this.idx, 1);
    const free = RING_FRAMES - 1 - this.available();
    const frames = Math.min(samples.length >> 1, free);
    for (let i = 0; i < frames; i++) {
      this.data[write * 2] = (samples[i * 2] ?? 0) / 32768;
      this.data[write * 2 + 1] = (samples[i * 2 + 1] ?? 0) / 32768;
      write = (write + 1) % RING_FRAMES;
    }
    Atomics.store(this.idx, 1, write);
  }

  /** Reads into two channels; pads with silence on underrun. */
  pull(left: Float32Array, right: Float32Array): void {
    let read = Atomics.load(this.idx, 0);
    const frames = Math.min(left.length, this.available());
    for (let i = 0; i < frames; i++) {
      left[i] = this.data[read * 2] ?? 0;
      right[i] = this.data[read * 2 + 1] ?? 0;
      read = (read + 1) % RING_FRAMES;
    }
    left.fill(0, frames);
    right.fill(0, frames);
    Atomics.store(this.idx, 0, read);
  }
}
