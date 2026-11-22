// Netplay, in the emulator worker: the members of a room run the same game
// in step (lockstep). Each sends its controller input for frame f + its
// delay before running frame f. A frame runs once the host's input for it
// is known (it carries who owns each controller) and the input of every
// owner: members who own no controller only watch, and nobody waits for
// them. The server routes messages (see the server's netplay.rs).
//
// Every CHECK_EVERY frames, members compare a checksum of the machine with
// the one of the member with the controls (the host when it has some):
// a member whose checksum differs asks it for its state, and loads it.
// Members joining get the host's state the same way.

import type { Emulator } from "./wasm/texel_web.js";
import type { Axes, ChatLine, Half, Member, NetStatus, Ports, SplitView } from "./protocol";

const OPPOSITE: Record<Half, Half> = { top: "bottom", bottom: "top", left: "right", right: "left" };

export const HOST_ID = 0;
const EVERYONE = 255;

// Binary message kinds, after the [to, from] header.
const INPUT = 1;
const SNAPSHOT = 2;
const CHECK = 3;

const CHECK_EVERY = 120;
/** Frames of input kept, to replay after loading another member's state. */
const HISTORY = 600;
const MAX_DELAY = 15;
/** Frames behind the host past which a member asks for its state. */
const TOO_FAR_BEHIND = 600;

interface Packet {
  buttons: number;
  axes: Axes;
  /** The Analog button was pressed. */
  analog: boolean;
  /** Controller owners: host packets only. */
  ports: Ports;
}

/** A little binary writer. */
class Writer {
  private buf = new Uint8Array(64);
  private view = new DataView(this.buf.buffer);
  length = 0;
  private room(n: number) {
    if (this.length + n <= this.buf.length) return;
    const bigger = new Uint8Array(Math.max(this.buf.length * 2, this.length + n));
    bigger.set(this.buf);
    this.buf = bigger;
    this.view = new DataView(bigger.buffer);
  }
  u8(v: number) {
    this.room(1);
    this.view.setUint8(this.length++, v);
    return this;
  }
  u16(v: number) {
    this.room(2);
    this.view.setUint16(this.length, v);
    this.length += 2;
    return this;
  }
  u32(v: number) {
    this.room(4);
    this.view.setUint32(this.length, v);
    this.length += 4;
    return this;
  }
  bytes(b: Uint8Array) {
    this.room(b.length);
    this.buf.set(b, this.length);
    this.length += b.length;
    return this;
  }
  done(): Uint8Array {
    return this.buf.slice(0, this.length);
  }
}

class Reader {
  private at = 0;
  constructor(private readonly view: DataView) {}
  u8() {
    return this.view.getUint8(this.at++);
  }
  u16() {
    const v = this.view.getUint16(this.at);
    this.at += 2;
    return v;
  }
  u32() {
    const v = this.view.getUint32(this.at);
    this.at += 4;
    return v;
  }
  bytes(n: number) {
    const b = new Uint8Array(this.view.buffer, this.view.byteOffset + this.at, n);
    this.at += n;
    return b;
  }
}

function writePacket(w: Writer, frame: number, p: Packet) {
  w.u32(frame).u16(p.buttons);
  p.axes.forEach((a) => w.u8(a));
  w.u8(p.analog ? 1 : 0);
  for (const ids of p.ports) {
    w.u8(ids.length);
    ids.forEach((id) => w.u8(id));
  }
}

function readPacket(r: Reader): [number, Packet] {
  const frame = r.u32();
  const buttons = r.u16();
  const axes: Axes = [r.u8(), r.u8(), r.u8(), r.u8()];
  const analog = r.u8() === 1;
  const ports = [0, 1].map(() => Array.from({ length: r.u8() }, () => r.u8())) as Ports;
  return [frame, { buttons, axes, analog, ports }];
}

const median = (xs: number[]) => (xs.length ? [...xs].sort((a, b) => a - b)[xs.length >> 1]! : null);
const owners = (ports: Ports) => [...new Set([...ports[0], ...ports[1]])];

export interface SessionHooks {
  emu: () => Emulator | null;
  /** Runs a frame for real: audio, pads, stats. */
  runFrame: () => void;
  status: (s: NetStatus) => void;
  chat: (line: ChatLine) => void;
  gzip: (data: Uint8Array, decompress?: boolean) => Promise<Uint8Array>;
  /** This member got the game's state and runs it. */
  onStarted?: () => void;
}

interface MemberState {
  name: string;
  playing: boolean;
  rtt: number | null;
}

export class Session {
  private readonly ws: WebSocket;
  private state: NetStatus["state"] = "connecting";
  private code: string | null = null;
  private me: number | null = null;
  private started = false;
  private frame = 0;
  private nextSend = 0;
  private delay = 1;
  /** Inputs by frame, then member. */
  private readonly inputs = new Map<number, Map<number, Packet>>();
  private readonly history = new Map<number, Map<number, Packet>>();
  /** The last frame the host's input is known for. */
  private latestHost = -1;
  private readonly members = new Map<number, MemberState>();
  /** Owners of the frame just run. */
  private ports: Ports = [[HOST_ID], []];
  /** Host only: the owners to send with the next input. */
  private wantedPorts: Ports = [[HOST_ID], []];
  private autoAssigned = false;
  /** Checksums by frame, then member. */
  private readonly checks = new Map<number, Map<number, number>>();
  private checkFloor = 0;
  private resyncAskedAt = 0;
  private pendingAnalog = false;
  private rtts: number[] = [];
  private timers: ReturnType<typeof setInterval>[] = [];
  private stalledSince = 0;
  private waitingFor: string[] = [];
  private note: string | undefined;
  /** This member's controller: the keyboard and first gamepad. */
  input: { buttons: number; axes: Axes } = { buttons: 0, axes: [128, 128, 128, 128] };

  constructor(
    readonly isHost: boolean,
    url: string,
    private readonly hooks: SessionHooks,
  ) {
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.onopen = () => {
      const ping = () => this.sendText({ type: "ping", t: performance.now() });
      ping();
      this.timers.push(setInterval(ping, 1000));
      this.timers.push(setInterval(() => this.adapt(), 2000));
    };
    this.ws.onmessage = (e: MessageEvent<string | ArrayBuffer>) => {
      if (typeof e.data === "string") this.onText(JSON.parse(e.data) as Record<string, unknown>);
      else void this.onBinary(new DataView(e.data));
    };
    this.ws.onclose = () => {
      this.timers.forEach(clearInterval);
      if (this.state !== "ended") {
        this.started = false;
        this.setState("ended", isHost ? "The room was closed." : "The connection to the room was lost.");
      }
    };
  }

  /** Running in step with the room. */
  get lockstep(): boolean {
    return this.started;
  }

  /** A guest has nothing to run before it gets the host's state. */
  get waiting(): boolean {
    return !this.isHost && !this.started;
  }

  /** Split screen, set by the host: display only, not part of the game. */
  private view: SplitView = null;

  /** The half of the screen to hide from this member: the other
   * player's, when it holds one controller and the host splits the view. */
  get hiddenHalf(): Half | null {
    if (!this.view || !this.started || this.me === null) return null;
    const mine = [0, 1].filter((port) => this.ports[port]!.includes(this.me!));
    if (mine.length !== 1) return null;
    const myHalf = mine[0] === 0 ? this.view : OPPOSITE[this.view];
    return OPPOSITE[myHalf];
  }

  setView(view: SplitView) {
    if (!this.isHost) return;
    this.view = view;
    this.sendText({ type: "view", view });
    this.report();
  }

  /** Frames to run quickly to catch up with the host (after a pause). */
  get behind(): number {
    return this.started ? Math.max(0, this.latestHost - MAX_DELAY - 2 - this.frame) : 0;
  }

  setPorts(ports: Ports) {
    if (!this.isHost) return;
    this.wantedPorts = ports;
    this.autoAssigned = true;
    this.report();
  }

  pressAnalog() {
    this.pendingAnalog = true;
  }

  say(text: string) {
    this.sendText({ type: "chat", text });
  }

  close() {
    this.started = false;
    this.setState("ended", this.isHost ? "You closed the room." : "You left the room.");
    this.timers.forEach(clearInterval);
    this.ws.close();
  }

  private sendText(msg: object) {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  /** Sends a binary message: [to, from] then the body. */
  private sendBinary(to: number, body: Uint8Array) {
    if (this.ws.readyState !== WebSocket.OPEN) return;
    const out = new Uint8Array(body.length + 2);
    out[0] = to;
    out.set(body, 2);
    this.ws.send(out);
  }

  private name(id: number): string {
    return this.members.get(id)?.name ?? `Player ${id}`;
  }

  private system(text: string) {
    this.hooks.chat({ from: null, name: "", text, at: Date.now() });
  }

  private noteTimer: ReturnType<typeof setTimeout> | undefined;

  /** A note that goes away after a while. */
  private flash(text: string) {
    this.note = text;
    this.report();
    clearTimeout(this.noteTimer);
    this.noteTimer = setTimeout(() => {
      if (this.note !== text) return;
      this.note = undefined;
      this.report();
    }, 10_000);
  }

  private setState(state: NetStatus["state"], note?: string) {
    this.state = state;
    if (note !== undefined) this.note = note || undefined;
    this.report();
  }

  private report() {
    const members: Member[] = [...this.members].map(([id, m]) => ({
      id,
      name: m.name,
      host: id === HOST_ID,
      me: id === this.me,
      playing: m.playing,
      rtt: id === this.me ? median(this.rtts) : m.rtt,
    }));
    this.hooks.status({
      state: this.state,
      code: this.code,
      me: this.me,
      members,
      ports: this.isHost ? this.wantedPorts : this.ports,
      rtt: median(this.rtts),
      delay: this.started ? this.delay : null,
      waitingFor: this.waitingFor,
      view: this.view,
      hidden: this.hiddenHalf,
      note: this.note,
    });
  }

  /** Sets this member's delay from the round trips to the server: input
   * goes through it, from this member to the slowest other. */
  private adapt() {
    const mine = median(this.rtts);
    if (mine === null || this.me === null) return;
    this.sendText({ type: "rtt", ms: Math.round(mine) });
    const others = [...this.members].filter(([id]) => id !== this.me).map(([, m]) => m.rtt ?? mine);
    const emu = this.hooks.emu();
    const frameMs = 1000 / (emu?.isPal ? 50 : 60);
    const delay = others.length ? Math.ceil((mine / 2 + Math.max(...others) / 2) / frameMs) + 1 : 1;
    this.delay = Math.min(MAX_DELAY, Math.max(1, delay));
    this.report();
  }

  private onText(msg: Record<string, unknown>) {
    const from = typeof msg.from === "number" ? msg.from : null;
    switch (msg.type) {
      case "welcome": {
        this.code = String(msg.code);
        this.me = Number(msg.you);
        for (const m of msg.members as { id: number; name: string }[]) {
          this.members.set(m.id, { name: m.name, playing: m.id !== this.me || this.isHost, rtt: null });
        }
        if (this.isHost) {
          // From now on, frames are counted and input goes through the room.
          this.started = true;
          this.frame = 0;
          this.nextSend = 0;
          this.setState("playing");
        } else {
          this.sendText({ type: "hello", to: HOST_ID });
          this.setState("syncing");
        }
        // After the status: the room's chat starts with its code.
        for (const line of msg.chat as ChatLine[]) this.hooks.chat(line);
        break;
      }
      case "joined": {
        const id = Number(msg.id);
        this.members.set(id, { name: String(msg.name), playing: false, rtt: null });
        if (this.isHost && this.view) this.sendText({ type: "view", to: id, view: this.view });
        this.system(`${String(msg.name)} joined.`);
        this.report();
        break;
      }
      case "left": {
        const id = Number(msg.id);
        const name = this.name(id);
        this.members.delete(id);
        if (this.isHost) {
          this.wantedPorts = this.wantedPorts.map((ids) => ids.filter((x) => x !== id)) as Ports;
        }
        this.system(`${name} left.`);
        this.report();
        break;
      }
      case "closed":
        this.started = false;
        this.system("The host closed the room.");
        this.setState("ended", "The host closed the room.");
        this.ws.close();
        break;
      case "hello":
        // A member wants the game's state, to start (or to catch up).
        if (this.isHost && from !== null) void this.sendSnapshot(from);
        break;
      case "resync":
        if (from !== null) {
          void this.sendSnapshot(from);
          this.flash(
            msg.reason === "behind"
              ? `${this.name(from)} was far behind: sent them the game's state.`
              : `${this.name(from)} drifted apart: sent them the game's state.`,
          );
        }
        break;
      case "playing":
        if (from !== null) {
          const m = this.members.get(from);
          if (m) m.playing = true;
          // The first guest to arrive gets the second controller.
          if (this.isHost && !this.autoAssigned && !this.wantedPorts[1].length) {
            this.wantedPorts = [this.wantedPorts[0], [from]];
            this.autoAssigned = true;
          }
          this.report();
        }
        break;
      case "view":
        if (from === HOST_ID) {
          const v = msg.view;
          this.view = v === "top" || v === "bottom" || v === "left" || v === "right" ? v : null;
          this.report();
        }
        break;
      case "rtt":
        if (from !== null) {
          const m = this.members.get(from);
          if (m) m.rtt = Number(msg.ms);
        }
        break;
      case "pong":
        if (typeof msg.t === "number") {
          this.rtts.push(performance.now() - msg.t);
          if (this.rtts.length > 10) this.rtts.shift();
          if (this.rtts.length === 1) this.adapt();
        }
        break;
      case "chat":
        this.hooks.chat({ from, name: String(msg.name), text: String(msg.text), at: Number(msg.at) });
        break;
    }
  }

  /** Sends the machine's state, between two frames, with the inputs known
   * for the frames ahead. */
  private async sendSnapshot(to: number) {
    const emu = this.hooks.emu();
    if (!emu || !this.started) return;
    const frame = this.frame;
    const state = emu.saveState();
    const cards = [0, 1].map((slot) => emu.memoryCard(slot) ?? new Uint8Array());
    const pending: [number, number, Packet][] = [];
    for (const [f, set] of this.inputs) if (f >= frame) for (const [id, p] of set) pending.push([id, f, p]);
    const gz = await this.hooks.gzip(state);
    const w = new Writer().u8(SNAPSHOT).u32(frame).u16(pending.length);
    for (const [id, f, p] of pending) {
      w.u8(id);
      writePacket(w, f, p);
    }
    w.u32(gz.length).bytes(gz);
    for (const c of cards) w.u32(c.length).bytes(c);
    this.sendBinary(to, w.done());
  }

  private store(id: number, frame: number, p: Packet) {
    let set = this.inputs.get(frame);
    if (!set) this.inputs.set(frame, (set = new Map()));
    if (!set.has(id)) set.set(id, p);
    if (id === HOST_ID && frame > this.latestHost) this.latestHost = frame;
  }

  private async onBinary(view: DataView) {
    const r = new Reader(view);
    r.u8(); // to
    const from = r.u8();
    switch (r.u8()) {
      case INPUT: {
        const [frame, p] = readPacket(r);
        if (!this.started || frame >= this.frame) this.store(from, frame, p);
        // Far behind (the tab was hidden): ask for the state.
        if (from === HOST_ID && this.started && frame - this.frame > TOO_FAR_BEHIND) this.askForState(HOST_ID, "behind");
        break;
      }
      case CHECK: {
        const frame = r.u32();
        const sum = r.u32();
        this.check(frame, from, sum);
        break;
      }
      case SNAPSHOT:
        await this.load(r);
        break;
    }
  }

  private askForState(to: number, reason: "behind" | "drift") {
    const now = performance.now();
    if (now - this.resyncAskedAt < 5000) return;
    this.resyncAskedAt = now;
    this.sendText({ type: "resync", to, reason });
  }

  /** Loads another member's state: to start, or to resynchronise. */
  private async load(r: Reader) {
    const frame = r.u32();
    const count = r.u16();
    const pending: [number, number, Packet][] = [];
    for (let i = 0; i < count; i++) {
      const id = r.u8();
      const [f, p] = readPacket(r);
      pending.push([id, f, p]);
    }
    const gz = r.bytes(r.u32());
    const cards = [0, 1].map(() => r.bytes(r.u32()));
    const emu = this.hooks.emu();
    if (!emu) return;
    const state = await this.hooks.gzip(gz, true);
    emu.loadState(state);
    cards.forEach((c, slot) => {
      if (c.length) emu.insertMemoryCard(slot, c);
    });
    for (const [id, f, p] of pending) this.store(id, f, p);
    this.checks.clear();
    this.checkFloor = frame;
    this.resyncAskedAt = 0;
    if (!this.started) {
      this.frame = frame;
      this.nextSend = frame;
      this.started = true;
      for (const f of this.inputs.keys()) if (f < frame) this.inputs.delete(f);
      this.sendText({ type: "playing" });
      const me = this.me !== null ? this.members.get(this.me) : undefined;
      if (me) me.playing = true;
      this.setState("playing", "");
      this.hooks.onStarted?.();
      return;
    }
    // Ahead of the state: replay the frames already run on it.
    for (let f = frame; f < this.frame; f++) {
      const set = this.history.get(f);
      if (!set) break;
      this.apply(emu, set);
      emu.runFrame();
      emu.takeAudio();
    }
    // Behind it: skip ahead. Inputs not sent yet are sent by step().
    if (this.frame < frame) {
      for (const f of this.inputs.keys()) if (f < frame) this.inputs.delete(f);
      this.frame = frame;
    }
    this.flash(`Got the game's state again, at frame ${frame}.`);
  }

  /** Runs the next frame if the inputs it needs are known. False when stalled. */
  step(): boolean {
    const emu = this.hooks.emu();
    if (!this.started || !emu || this.me === null) return false;
    const f = this.frame;
    while (this.nextSend <= f + this.delay) {
      const p: Packet = {
        buttons: this.input.buttons,
        axes: [...this.input.axes],
        analog: this.pendingAnalog,
        ports: this.isHost ? this.wantedPorts : [[], []],
      };
      this.pendingAnalog = false;
      this.store(this.me, this.nextSend, p);
      const w = new Writer().u8(INPUT);
      writePacket(w, this.nextSend, p);
      this.sendBinary(EVERYONE, w.done());
      this.nextSend++;
    }
    const set = this.inputs.get(f);
    const host = set?.get(HOST_ID);
    // Members who left send nothing more: their controllers go idle.
    const missing = host ? owners(host.ports).filter((id) => !set!.has(id) && this.members.has(id)) : [HOST_ID];
    if (!set || missing.length) {
      const now = performance.now();
      if (!this.stalledSince) this.stalledSince = now;
      else if (now - this.stalledSince > 500 && this.state === "playing") {
        this.waitingFor = missing.map((id) => this.name(id));
        this.setState("stalled");
      }
      return false;
    }
    this.stalledSince = 0;
    if (this.state === "stalled") {
      this.waitingFor = [];
      this.setState("playing");
    }
    this.inputs.delete(f);
    this.history.set(f, set);
    this.history.delete(f - HISTORY);
    this.apply(emu, set);
    this.hooks.runFrame();
    this.frame = f + 1;
    if (f % CHECK_EVERY === 0) {
      const sum = emu.stateChecksum();
      this.sendBinary(EVERYONE, new Writer().u8(CHECK).u32(f).u32(sum).done());
      this.check(f, this.me, sum);
    }
    return true;
  }

  /** Sets the controllers from their owners' input. */
  private apply(emu: Emulator, set: Map<number, Packet>) {
    const ports = set.get(HOST_ID)!.ports;
    if (JSON.stringify(ports) !== JSON.stringify(this.ports)) {
      this.ports = ports;
      this.report();
    }
    ports.forEach((ids, port) => {
      const from = ids.map((id) => set.get(id)).filter((p): p is Packet => p !== undefined);
      const buttons = from.reduce((m, p) => m | p.buttons, 0);
      // Shared controller: each stick axis follows whoever pushes it furthest.
      const axes = [0, 1, 2, 3].map((i) =>
        from.reduce((best, p) => (Math.abs(p.axes[i]! - 128) > Math.abs(best - 128) ? p.axes[i]! : best), 128),
      ) as Axes;
      emu.setButtons(port, buttons);
      emu.setAxes(port, axes[0], axes[1], axes[2], axes[3]);
      if (from.some((p) => p.analog)) emu.pressAnalogButton(port);
    });
  }

  /** Who has the last word on the machine's state at a frame: the host if
   * it has a controller, else the owner of the first controller, then of
   * the second, else the host. */
  private authority(frame: number): number {
    const ports = this.history.get(frame)?.get(HOST_ID)?.ports ?? this.ports;
    const list = owners(ports).filter((id) => this.members.has(id));
    return list.includes(HOST_ID) ? HOST_ID : (list[0] ?? HOST_ID);
  }

  private check(frame: number, id: number, sum: number) {
    if (frame < this.checkFloor || this.me === null) return;
    let sums = this.checks.get(frame);
    if (!sums) this.checks.set(frame, (sums = new Map()));
    sums.set(id, sum);
    for (const f of this.checks.keys()) if (f < frame - 4 * CHECK_EVERY) this.checks.delete(f);
    const authority = this.authority(frame);
    if (authority === this.me) return;
    const mine = sums.get(this.me);
    const theirs = sums.get(authority);
    if (mine === undefined || theirs === undefined || mine === theirs) return;
    this.flash(`Drifted apart from ${this.name(authority)} at frame ${frame}: asking for their state.`);
    this.askForState(authority, "drift");
  }
}
