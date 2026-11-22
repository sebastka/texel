// Typed client of the server API.

/** The console a game or a BIOS belongs to. */
export type Console = "ps1" | "ps2";

export interface Bios {
  id: string;
  console: Console;
  file: string;
  sha1: string;
  version: string | null;
  date: string | null;
  /** "any": region-free (the PSP's PS1 emulator). The last two are PS2 only. */
  region: "NTSC-U" | "PAL" | "NTSC-J" | "any" | "unknown" | "NTSC-Asia" | "NTSC-China";
  /** Redump's description, when the image is a known-good dump. */
  model: string | null;
  /** A known-good dump; null until Redump's DAT is downloaded. */
  verified: boolean | null;
  /** Who lists it as good: Redump, or libretro for BIOSes Redump does not cover. */
  verifiedBy: "Redump" | "libretro" | null;
  /** What a non-console BIOS comes from, e.g. "PSP firmware 6.60", "OpenBIOS", "OpenPS2BIOS". */
  variant: string | null;
  /** For OpenBIOS (OpenPS2BIOS): the PCSX-Redux (OpenPS2BIOS) commit it was built from, when known. */
  commit: string | null;
}

export interface Game {
  id: string;
  console: Console;
  path: string;
  serial: string | null;
  name: string | null;
  title: string;
  tags: string[];
  /** true: matches Redump; false: hashed but unknown; null: not checked yet. */
  verified: boolean | null;
  error: string | null;
  /** Disc number of a multi-disc game. */
  disc: number | null;
  /** Every disc of a multi-disc game, this one included; empty otherwise. */
  discs: { id: string; number: number }[];
}

export interface Details {
  summary: string | null;
  summarySource: "Wikipedia" | "IGDB" | null;
  /** e.g. "1997 sim racing video game". */
  tagline: string | null;
  genres: string[];
  developers: string[];
  publishers: string[];
  modes: string[];
  /** YYYY-MM-DD, YYYY-MM or YYYY. */
  releaseDate: string | null;
  /** Out of 100. */
  rating: number | null;
  wikipediaUrl: string | null;
  igdbUrl: string | null;
}

export interface GameListItem extends Game {
  details: Details | null;
  size: number;
}

export interface FileCheck {
  name: string;
  size: number;
  sha1: string | null;
  redumpName: string | null;
  redumpSha1: string | null;
  matches: boolean | null;
}

export interface GameDetail extends Game {
  volumeId: string | null;
  cue: string | null;
  files: FileCheck[];
  redumpTracks: number | null;
  details: Details | null;
  /** The BIOS the game was last started with, or its save states use. */
  lastBios: string | null;
  /** The disc has an SBI file: the subchannel data of its LibCrypt protection. */
  sbi: boolean;
  /** Other pages about the game, from its YAML file: name → URL. */
  links: Record<string, string>;
  /** Why the game's YAML file was not used, when it is wrong. */
  overridesError: string | null;
}

/** A request the server refused: its status, and its message. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

async function check(resp: Response): Promise<Response> {
  if (!resp.ok) throw new ApiError(resp.status, (await resp.text()) || `HTTP ${resp.status}`);
  return resp;
}

async function json<T>(url: string): Promise<T> {
  return (await (await check(await fetch(url))).json()) as T;
}

/** Sends JSON (or nothing); the answer, as JSON when there is one. */
async function send<T = void>(method: string, url: string, body?: unknown): Promise<T> {
  const resp = await check(
    await fetch(url, {
      method,
      headers: body === undefined ? {} : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  );
  const text = await resp.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

export type Role = "admin" | "guest";

export interface User {
  id: number;
  name: string;
  role: Role;
  /** Invited users': what they sign in with. */
  email: string | null;
  /** Signed in with OpenID Connect: name and password are the provider's. */
  oidc: boolean;
}

export interface AuthConfig {
  /** The OpenID Connect provider's name, when sign-in with it is offered. */
  oidc: string | null;
}

export interface Invite {
  kind: "invite" | "reset";
  note: string;
  /** A reset's account. */
  email: string | null;
}

export interface UserRow {
  id: number;
  name: string;
  role: Role;
  kind: "oidc" | "password";
  email: string | null;
  createdAt: number;
  lastSeenAt: number | null;
}

export interface InviteRow {
  id: number;
  kind: "invite" | "reset";
  note: string;
  user: string | null;
  createdBy: string | null;
  createdAt: number;
  expiresAt: number;
  usedAt: number | null;
  usedBy: string | null;
}

export interface CardSave {
  /** Its name on the card: what designates it. */
  id: string;
  title: string;
  size: number;
  /** PlayStation saves: the icon's frames, 16×16 RGBA, base64. */
  icon?: string[];
}

export interface Card {
  game: { id: string; title: string; console: Console; serial: string | null } | null;
  key: string;
  slot: number;
  console: Console;
  formatted?: boolean;
  saves?: CardSave[];
  free?: number;
  capacity?: number;
  /** Why the card could not be read. */
  error: string | null;
}

export interface SaveState {
  slot: number;
  size: number;
  /** Unix seconds. */
  savedAt: number;
  screenshot: boolean;
  /** The BIOS it was saved with: it loads only with it. */
  bios: string | null;
}

export const api = {
  /** The signed-in user, or null. */
  me: async (): Promise<User | null> => {
    try {
      return await json<User>("/api/me");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return null;
      throw e;
    }
  },
  authConfig: () => json<AuthConfig>("/api/auth/config"),
  login: (email: string, password: string) => send<User>("POST", "/api/auth/login", { email, password }),
  invite: (token: string) => json<Invite>(`/api/auth/invites/${encodeURIComponent(token)}`),
  register: (r: { token: string; email?: string; name?: string; password: string }) =>
    send<User>("POST", "/api/auth/register", r),
  logout: () => send<{ redirect: string | null }>("POST", "/api/auth/logout"),
  updateMe: (u: { name?: string; password?: string; currentPassword?: string }) => send<User>("PATCH", "/api/me", u),
  users: () => json<UserRow[]>("/api/admin/users"),
  deleteUser: (id: number) => send("DELETE", `/api/admin/users/${id}`),
  resetUser: (id: number) => send<{ id: number; token: string }>("POST", `/api/admin/users/${id}/reset`),
  invites: () => json<InviteRow[]>("/api/admin/invites"),
  createInvite: (note: string) => send<{ id: number; token: string }>("POST", "/api/admin/invites", { note }),
  deleteInvite: (id: number) => send("DELETE", `/api/admin/invites/${id}`),
  cards: () => json<Card[]>("/api/cards"),
  deleteCard: (game: string, slot: number) => send("DELETE", `/api/cards/${encodeURIComponent(game)}/${slot}`),
  deleteSave: (game: string, slot: number, save: string) =>
    send("DELETE", `/api/cards/${encodeURIComponent(game)}/${slot}/saves/${encodeURIComponent(save)}`),
  copySave: (game: string, slot: number, save: string, toGame: string, toSlot: number, move: boolean) =>
    send("POST", `/api/cards/${encodeURIComponent(game)}/${slot}/saves/${encodeURIComponent(save)}/copy`, {
      toGame,
      toSlot,
      move,
    }),
  /** A whole card image: replaces the game's card. */
  putCard: (game: string, slot: number, data: ArrayBuffer) =>
    fetch(`/api/games/${encodeURIComponent(game)}/memcards/${slot}`, { method: "PUT", body: data }).then(check),
  cardUrl: (game: string, slot: number) => `/api/games/${encodeURIComponent(game)}/memcards/${slot}`,
  states: (id: string) => json<SaveState[]>(`/api/games/${encodeURIComponent(id)}/states`),
  bios: () => json<Bios[]>("/api/bios"),
  games: () => json<GameListItem[]>("/api/games"),
  /** The games with saved memory cards (a game on several discs: its first). */
  memcards: () => json<string[]>("/api/memcards"),
  game: (id: string) => json<GameDetail>(`/api/games/${encodeURIComponent(id)}`),
  room: (code: string) =>
    json<{ game: string; bios: string; full: boolean }>(`/api/netplay/rooms/${encodeURIComponent(code)}`),
  /** Records the BIOS a game is started with, preselected next time. */
  setBios: (id: string, bios: string) =>
    fetch(`/api/games/${encodeURIComponent(id)}/bios`, { method: "PUT", body: bios }),
  rescan: () => fetch("/api/library/rescan", { method: "POST" }).then(check),
};

export const image = (id: string, kind: "boxart" | "title" | "snap") =>
  `/api/games/${encodeURIComponent(id)}/images/${kind}`;

/** The region of a game, from its Redump tags or its serial. */
export function gameRegion(game: Game): Bios["region"] | null {
  const tags = game.tags.join(" ");
  if (/\b(Europe|Germany|France|Spain|Italy|UK|Australia|Netherlands|Sweden|Scandinavia)\b/.test(tags)) return "PAL";
  if (/\b(USA|Canada)\b/.test(tags)) return "NTSC-U";
  if (/\b(Japan|Asia|Korea)\b/.test(tags)) return "NTSC-J";
  const prefix = game.serial?.slice(0, 4) ?? "";
  if (/^S[CL]E[SD]$/.test(prefix)) return "PAL";
  if (/^S[CL]U[SD]$/.test(prefix)) return "NTSC-U";
  if (/^S[CL]P[SM]$|^SLPM$|^SCPS$/.test(prefix)) return "NTSC-J";
  return null;
}
