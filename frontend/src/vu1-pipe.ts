// VU1 in a worker of its own (see the core's vu1q): the emulation worker
// sends it batches of operations and, at a sync, waits for what they sent the
// GIF (records). Both go through pipes in SharedArrayBuffers, waited on with
// Atomics.wait: the emulation worker blocks at a sync, as the core expects
// (the records are replayed before the EE goes on), and VU1's worker blocks
// between batches.
//
// No imports and only erasable types: Node runs this file as it is, for the
// checks in scripts/.

/** Header slots (Int32): the reader's and writer's offsets, a counter bumped
 * on every change (what both sides wait on); on the records pipe, batches
 * done and whether VU1's worker failed. */
const READ = 0;
const WRITE = 1;
const SIGNAL = 2;
const DONE = 3;
const FAILED = 4;
/** VU1's worker's time running batches (ms, rounded), for diagnostics. */
const BUSY = 5;
const HEADER = 32;

/** One-way bytes, one writer, one reader. */
export class Pipe {
  readonly h: Int32Array;
  readonly data: Uint8Array;

  static create(bytes: number): SharedArrayBuffer {
    return new SharedArrayBuffer(HEADER + bytes);
  }

  constructor(buffer: SharedArrayBuffer) {
    this.h = new Int32Array(buffer, 0, HEADER / 4);
    this.data = new Uint8Array(buffer, HEADER);
  }

  signal(): number {
    return Atomics.load(this.h, SIGNAL);
  }

  /** Waits for the other side to change something (from `seen`). */
  waitSignal(seen: number, ms = Infinity): void {
    Atomics.wait(this.h, SIGNAL, seen, ms);
  }

  bump(): void {
    Atomics.add(this.h, SIGNAL, 1);
    Atomics.notify(this.h, SIGNAL);
  }

  used(): number {
    const cap = this.data.length;
    return (Atomics.load(this.h, WRITE) - Atomics.load(this.h, READ) + cap) % cap;
  }

  /** Writes what fits of `src` (one byte stays free): how much. */
  put(src: Uint8Array): number {
    const cap = this.data.length;
    const n = Math.min(cap - 1 - this.used(), src.length);
    if (n === 0) return 0;
    const w = Atomics.load(this.h, WRITE);
    const first = Math.min(n, cap - w);
    this.data.set(src.subarray(0, first), w);
    this.data.set(src.subarray(first, n), 0);
    Atomics.store(this.h, WRITE, (w + n) % cap);
    this.bump();
    return n;
  }

  /** Reads up to `dst`'s length: how much. */
  take(dst: Uint8Array): number {
    const cap = this.data.length;
    const n = Math.min(this.used(), dst.length);
    if (n === 0) return 0;
    const r = Atomics.load(this.h, READ);
    const first = Math.min(n, cap - r);
    dst.set(this.data.subarray(r, r + first), 0);
    dst.set(this.data.subarray(0, n - first), first);
    Atomics.store(this.h, READ, (r + n) % cap);
    this.bump();
    return n;
  }

  /** Writes all of `src`, waiting for room (`stalled`: called meanwhile). */
  write(src: Uint8Array, stalled?: () => void): void {
    let off = 0;
    while (off < src.length) {
      const seen = this.signal();
      const n = this.put(src.subarray(off));
      off += n;
      if (n === 0) {
        stalled?.();
        this.waitSignal(seen, stalled ? 1 : Infinity);
      }
    }
  }

  /** Reads exactly `dst`'s length, waiting for it. */
  read(dst: Uint8Array): void {
    let off = 0;
    while (off < dst.length) {
      const seen = this.signal();
      const n = this.take(dst.subarray(off));
      off += n;
      if (n === 0) this.waitSignal(seen);
    }
  }
}

/** Pipe sizes: batches are about 64 KiB; records can be ten times that. */
export const OPS_BYTES = 4 << 20;
export const RECORDS_BYTES = 16 << 20;

/** The emulation worker's end: the core's `setVu1Worker(send, wait)`. */
export class Vu1Link {
  readonly ops: Pipe;
  readonly records: Pipe;
  private sent = 0;
  /** Time spent sending and waiting (ms), for diagnostics. */
  sendMs = 0;
  waitMs = 0;
  /** Records drained, not handed over yet. */
  private got = new Uint8Array(1 << 20);
  private len = 0;

  constructor(ops: SharedArrayBuffer, records: SharedArrayBuffer) {
    this.ops = new Pipe(ops);
    this.records = new Pipe(records);
  }

  /** A batch (its length first). */
  send(batch: Uint8Array): void {
    const started = performance.now();
    const head = new Uint8Array(4);
    new DataView(head.buffer).setUint32(0, batch.length, true);
    // While the ops pipe is full, the records' must not be: VU1's worker
    // could be waiting for room there.
    const drain = () => {
      this.check();
      this.drain();
    };
    this.ops.write(head, drain);
    this.ops.write(batch, drain);
    this.sent++;
    this.sendMs += performance.now() - started;
  }

  /** Every batch sent run: their records, in order (valid until the next
   * call: the core copies them at once). */
  wait(): Uint8Array {
    const started = performance.now();
    for (;;) {
      const seen = this.records.signal();
      this.check();
      this.drain();
      if (Atomics.load(this.records.h, DONE) === this.sent) {
        // Each batch's records are written before it counts as done.
        this.drain();
        break;
      }
      this.records.waitSignal(seen);
    }
    const out = this.got.subarray(0, this.len);
    this.len = 0;
    this.waitMs += performance.now() - started;
    return out;
  }

  /** VU1's worker's time running batches (ms). */
  busyMs(): number {
    return Atomics.load(this.records.h, BUSY);
  }

  private check(): void {
    if (Atomics.load(this.records.h, FAILED)) throw new Error("VU1's worker failed");
  }

  private drain(): void {
    for (;;) {
      const room = this.got.length - this.len;
      if (room < this.records.used()) {
        const bigger = new Uint8Array(Math.max(this.got.length * 2, this.len + this.records.used()));
        bigger.set(this.got.subarray(0, this.len));
        this.got = bigger;
      }
      const n = this.records.take(this.got.subarray(this.len));
      this.len += n;
      if (n === 0) return;
    }
  }
}

/** VU1's worker's end: runs batches as they come, forever (until one
 * fails: the emulation worker's wait throws then). */
export function serve(ops: SharedArrayBuffer, records: SharedArrayBuffer, process: (batch: Uint8Array) => Uint8Array): never {
  const input = new Pipe(ops);
  const output = new Pipe(records);
  const head = new Uint8Array(4);
  let busy = 0;
  try {
    for (;;) {
      input.read(head);
      const batch = new Uint8Array(new DataView(head.buffer).getUint32(0, true));
      input.read(batch);
      const started = performance.now();
      const out = process(batch);
      busy += performance.now() - started;
      Atomics.store(output.h, BUSY, Math.round(busy));
      output.write(out);
      Atomics.add(output.h, DONE, 1);
      output.bump();
    }
  } catch (e) {
    fail(records);
    throw e;
  }
}

/** VU1's worker could not go on. */
export function fail(records: SharedArrayBuffer): void {
  const output = new Pipe(records);
  Atomics.store(output.h, FAILED, 1);
  output.bump();
}
