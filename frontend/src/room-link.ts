// The netplay room, in the page: its connection, its members and chat, and
// the game it plays (none while the host chooses one). It outlives pages:
// the host browses the library in it, and the others follow the games the
// host starts. While a game runs, the player's worker runs it in step
// (netplay.ts): the room relays the game's messages, both ways.
//
// Where the server offers it (an administrators' setting), inputs also go
// as datagrams, over a WebTransport session next to the WebSocket: they
// come sooner when packets are lost (see the server's webtransport.rs).

import type { ChatLine, CodeSet, Ports, RoomSnapshot, SplitView } from "./protocol";

export interface RoomMember {
  id: number;
  name: string;
  /** Signed in: may take over as the host. */
  registered: boolean;
  /** Round trip to the server, in ms. */
  rtt: number | null;
}

export interface RoomGame {
  game: string;
  bios: string;
  /** The cheat and patch codes the host plays with: everyone's. */
  codes?: CodeSet | null;
}

export interface RoomEvents {
  /** Anything shown changed. */
  change: () => void;
  chat: (line: ChatLine) => void;
  /** In the room: this member and the room's state are known. */
  welcome: () => void;
  /** The host started a game, or stopped it (null). */
  game: (game: RoomGame | null) => void;
  /** Another member hosts now (this one, maybe). */
  host: () => void;
  /** Out of the room, and why. */
  end: (why: string) => void;
}

/** Netplay's datagrams: not offered (or no WebTransport here), opening,
 * open, or failed (UDP blocked, maybe: the WebSocket carries everything). */
export type Datagrams = "off" | "opening" | "on" | "failed";

const median = (xs: number[]) => (xs.length ? [...xs].sort((a, b) => a - b)[xs.length >> 1]! : null);

export function wsUrl(path: string): string {
  const url = new URL(path, location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.href;
}

export class Room {
  private readonly ws: WebSocket;
  state: "connecting" | "open" | "ended" = "connecting";
  code: string | null = null;
  me: number | null = null;
  host = 0;
  /** Counts the changes of game and host (see netplay.ts). */
  epoch = 0;
  game: RoomGame | null = null;
  readonly members = new Map<number, RoomMember>();
  /** Who has the controllers, as the host set them (or the game ran them). */
  ports: Ports = [[0], []];
  assigned = false;
  view: SplitView = null;
  /** Members run ahead and go back on a late input (see netplay.ts): the
   * server's setting when the room was opened. */
  rollback = false;
  datagrams: Datagrams = "off";
  private transport: WebTransport | null = null;
  private datagramWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private rtts: number[] = [];
  private readonly timers: ReturnType<typeof setInterval>[] = [];
  private leaving = false;
  /** Where the game's messages go: the running game's player. */
  relay: ((data: string | ArrayBuffer) => void) | null = null;

  constructor(
    url: string,
    private readonly on: RoomEvents,
  ) {
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.onopen = () => {
      // The server drops members silent for long: a ping a second.
      const ping = () => this.sendText({ type: "ping", t: performance.now() });
      ping();
      this.timers.push(setInterval(ping, 1000));
      this.timers.push(
        setInterval(() => {
          const mine = median(this.rtts);
          if (mine !== null) this.sendText({ type: "rtt", ms: Math.round(mine) });
        }, 2000),
      );
    };
    this.ws.onmessage = (e: MessageEvent<string | ArrayBuffer>) => {
      if (typeof e.data === "string") this.onText(e.data);
      else this.relay?.(e.data);
    };
    this.ws.onclose = () => this.end(this.leaving ? "" : "The connection to the room was lost.");
    window.addEventListener("pagehide", this.onPageHide);
  }

  /** Leaving the page (it may stay frozen in the back/forward cache):
   * leave the room, so that the others know. */
  private onPageHide = () => this.leave();

  get isHost(): boolean {
    return this.me !== null && this.me === this.host;
  }

  get hostName(): string {
    return this.members.get(this.host)?.name ?? "The host";
  }

  /** Who takes over when the host leaves: the first other member signed
   * in (as the server picks). */
  get heir(): RoomMember | undefined {
    return [...this.members.values()].sort((a, b) => a.id - b.id).find((m) => m.id !== this.host && m.registered);
  }

  /** What a player needs to run the room's game. */
  snapshot(): RoomSnapshot {
    return {
      me: this.me ?? 0,
      host: this.host,
      epoch: this.epoch,
      members: [...this.members.values()].map((m) => ({ ...m, rtt: m.id === this.me ? median(this.rtts) : m.rtt })),
      ports: this.ports,
      assigned: this.assigned,
      view: this.view,
      rollback: this.rollback,
    };
  }

  /** Host: plays this game, or none (everyone follows). */
  load(game: RoomGame | null) {
    if (this.isHost) this.sendText({ type: "load", game: game?.game ?? null, bios: game?.bios ?? null, codes: game?.codes ?? null });
  }

  /** This member left the game (not the room): nobody waits for it. */
  idle() {
    this.sendText({ type: "idle", epoch: this.epoch });
  }

  say(text: string) {
    this.sendText({ type: "chat", text });
  }

  /** Host: who has the controllers (the running game gets them too). */
  setPorts(ports: Ports) {
    this.ports = ports;
    this.assigned = true;
    this.on.change();
  }

  /** Host: split screen, for everyone. */
  setView(view: SplitView) {
    this.view = view;
    this.sendText({ type: "view", view });
    this.on.change();
  }

  /** A message from the running game. */
  send(data: string | Uint8Array) {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(data as string | Uint8Array<ArrayBuffer>);
  }

  /** A datagram from the running game: lost when there is no session. */
  sendDatagram(data: Uint8Array) {
    void this.datagramWriter?.write(data).catch(() => {});
  }

  /** Opens the datagram session the server offers, if this browser can. */
  private async openDatagrams(port: number, path: string) {
    if (typeof WebTransport === "undefined") return;
    this.datagrams = "opening";
    this.on.change();
    try {
      const transport = new WebTransport(`https://${location.hostname}:${port}${path}`);
      this.transport = transport;
      await transport.ready;
      if (this.state === "ended") {
        transport.close();
        return;
      }
      this.datagramWriter = transport.datagrams.writable.getWriter();
      this.datagrams = "on";
      this.on.change();
      const reader = transport.datagrams.readable.getReader();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        this.relay?.((value as Uint8Array).slice().buffer);
      }
    } catch {
      // Refused, or UDP blocked: the WebSocket carries everything anyway.
    }
    this.datagramWriter = null;
    this.transport = null;
    if (this.state !== "ended") {
      this.datagrams = "failed";
      this.on.change();
    }
  }

  leave() {
    if (this.state === "ended") return;
    this.leaving = true;
    this.end("");
  }

  private sendText(msg: object) {
    this.send(JSON.stringify(msg));
  }

  private notice(text: string) {
    this.on.chat({ from: null, name: "", text, at: Date.now() });
  }

  private name(id: number): string {
    return this.members.get(id)?.name ?? `Player ${id}`;
  }

  private end(why: string) {
    if (this.state === "ended") return;
    this.state = "ended";
    this.timers.forEach(clearInterval);
    window.removeEventListener("pagehide", this.onPageHide);
    this.ws.close();
    this.transport?.close();
    this.relay = null;
    this.on.end(why);
  }

  private onText(raw: string) {
    const msg = JSON.parse(raw) as Record<string, unknown>;
    const from = typeof msg.from === "number" ? msg.from : null;
    switch (msg.type) {
      case "welcome": {
        this.state = "open";
        this.code = String(msg.code);
        this.me = Number(msg.you);
        this.host = Number(msg.host);
        this.epoch = Number(msg.epoch);
        this.rollback = msg.rollback === true;
        const dg = msg.datagrams as { port: number; path: string } | null;
        if (dg && this.datagrams === "off") void this.openDatagrams(dg.port, dg.path);
        this.game = typeof msg.game === "string" ? { game: msg.game, bios: String(msg.bios), codes: (msg.codes as CodeSet | null) ?? null } : null;
        this.ports = [[this.host], []];
        for (const m of msg.members as { id: number; name: string; registered: boolean }[]) {
          this.members.set(m.id, { ...m, rtt: null });
        }
        for (const line of msg.chat as ChatLine[]) this.on.chat(line);
        this.on.welcome();
        this.on.change();
        return;
      }
      case "chat":
        this.on.chat({ from, name: String(msg.name), text: String(msg.text), at: Number(msg.at) });
        return;
      case "pong":
        if (typeof msg.t === "number") {
          this.rtts.push(performance.now() - msg.t);
          if (this.rtts.length > 10) this.rtts.shift();
        }
        break;
      case "rtt":
        if (from !== null) {
          const m = this.members.get(from);
          if (m) m.rtt = Number(msg.ms);
        }
        break;
      case "error":
        this.notice(String(msg.text));
        break;
    }
    // The game's part of it, for the running game.
    this.relay?.(raw);
    switch (msg.type) {
      case "joined": {
        const id = Number(msg.id);
        this.members.set(id, { id, name: String(msg.name), registered: msg.registered === true, rtt: null });
        if (this.isHost && this.view) this.sendText({ type: "view", to: id, view: this.view });
        this.notice(`${String(msg.name)} joined.`);
        break;
      }
      case "left": {
        const id = Number(msg.id);
        this.notice(`${this.name(id)} left.`);
        this.members.delete(id);
        this.ports = this.ports.map((ids) => ids.filter((x) => x !== id)) as Ports;
        break;
      }
      case "host":
        this.host = Number(msg.id);
        this.epoch = Number(msg.epoch);
        this.notice(this.isHost ? "You are now the host." : `${this.name(this.host)} is now the host.`);
        this.on.host();
        break;
      case "game":
        this.epoch = Number(msg.epoch);
        this.game = typeof msg.game === "string" ? { game: msg.game, bios: String(msg.bios), codes: (msg.codes as CodeSet | null) ?? null } : null;
        this.on.game(this.game);
        break;
      case "view":
        if (from === this.host) {
          const v = msg.view;
          this.view = v === "top" || v === "bottom" || v === "left" || v === "right" ? v : null;
        }
        break;
      case "closed":
        this.end("The host closed the room.");
        return;
      default:
        return;
    }
    this.on.change();
  }
}
