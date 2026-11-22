// Turning metadata into words: tags, languages, regions, BIOS descriptions
// and what verification means.

import { gameRegion, type Bios, type Game, type GameDetail } from "./api";

const LANGUAGES: Record<string, string> = {
  En: "English",
  Fr: "French",
  De: "German",
  Es: "Spanish",
  It: "Italian",
  Nl: "Dutch",
  Pt: "Portuguese",
  Sv: "Swedish",
  No: "Norwegian",
  Da: "Danish",
  Fi: "Finnish",
  Pl: "Polish",
  Ru: "Russian",
  Ja: "Japanese",
  Ko: "Korean",
  Zh: "Chinese",
  Ca: "Catalan",
  El: "Greek",
  Cs: "Czech",
  Hu: "Hungarian",
  Tr: "Turkish",
};

const REGION_NAMES: Record<Bios["region"], string> = {
  "NTSC-U": "North America (NTSC-U)",
  PAL: "Europe and Australia (PAL)",
  "NTSC-J": "Japan and Asia (NTSC-J)",
  any: "every region (region-free)",
  unknown: "unknown region",
  "NTSC-Asia": "Asia (NTSC)",
  "NTSC-China": "China (NTSC)",
};

/** Notes on Redump tags other than regions and languages. */
const TAG_NOTES: Record<string, string> = {
  EDC: "A pressing variant: some sectors carry error-detection codes that other pressings lack. Redump lists it separately.",
  Demo: "A demo disc.",
  Beta: "A pre-release version.",
  Proto: "A prototype.",
};

export interface Tags {
  regions: string[];
  languages: string[];
  other: { tag: string; note: string | null }[];
}

export function parseTags(tags: string[]): Tags {
  const out: Tags = { regions: [], languages: [], other: [] };
  for (const tag of tags) {
    const parts = tag.split(",").map((p) => p.trim());
    if (parts.every((p) => p in LANGUAGES)) {
      out.languages.push(...parts.map((p) => LANGUAGES[p] ?? p));
    } else if (/^(Europe|USA|Japan|Asia|Korea|World|Australia|Canada|Brazil|Germany|France|Spain|Italy|UK|Netherlands|Sweden|Scandinavia|Russia|China|Taiwan)(, .+)*$/.test(tag)) {
      out.regions.push(...parts);
    } else {
      out.other.push({ tag, note: TAG_NOTES[tag] ?? null });
    }
  }
  // Single-language releases have no language tag: infer it.
  if (!out.languages.length) {
    if (out.regions.includes("USA") || out.regions.includes("UK") || out.regions.includes("Australia")) out.languages.push("English");
    if (out.regions.includes("Japan")) out.languages.push("Japanese");
  }
  return out;
}

export function regionName(region: Bios["region"]): string {
  return REGION_NAMES[region];
}

export function year(date: string | null | undefined): string {
  return date?.slice(0, 4) ?? "";
}

/** Dates are shown as YYYY-MM-DD (or YYYY-MM, YYYY when less is known). */
export function formatDate(date: string | null | undefined): string | null {
  if (!date) return null;
  const [y, m, d] = date.split("-").map(Number);
  if (!y) return null;
  const pad = (n: number) => String(n).padStart(2, "0");
  return [String(y).padStart(4, "0"), m ? pad(m) : null, m && d ? pad(d) : null].filter(Boolean).join("-");
}

/** A moment, in local time: "YYYY-MM-DD HH:MM". */
export function formatDateTime(when: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${when.getFullYear()}-${pad(when.getMonth() + 1)}-${pad(when.getDate())} ${pad(when.getHours())}:${pad(when.getMinutes())}`;
}

export function formatSize(bytes: number): string {
  if (bytes >= 2 ** 30) return `${(bytes / 2 ** 30).toFixed(1)} GiB`;
  if (bytes >= 2 ** 20) return `${(bytes / 2 ** 20).toFixed(0)} MiB`;
  return `${(bytes / 2 ** 10).toFixed(0)} KiB`;
}

/** The first sentence of a summary, for lists. */
export function firstSentence(text: string | null | undefined): string {
  if (!text) return "";
  const m = /^(.+?[.!?])(\s|$)/.exec(text);
  return m?.[1] ?? text;
}

// --- Verification ------------------------------------------------------------

export type VerifyState = "verified" | "mismatch" | "pending" | "unidentified" | "unreadable";

export function verifyState(g: Game): VerifyState {
  if (g.error) return "unreadable";
  if (g.verified === true) return "verified";
  if (g.verified === false) return g.name ? "mismatch" : "unidentified";
  return "pending";
}

export const VERIFY_LABELS: Record<VerifyState, string> = {
  verified: "Verified",
  mismatch: "Modified or bad dump",
  pending: "Checking…",
  unidentified: "Unknown to Redump",
  unreadable: "Unreadable",
};

export const REDUMP_EXPLAINED =
  "Redump is a preservation project that catalogues retail discs: every disc is dumped by several people with " +
  "different drives, and listed only once the dumps match, with the size and checksums of each track.";

export function verifyExplanation(g: GameDetail): string {
  const tracks = g.files.length;
  const t = tracks === 1 ? "its track" : `all ${tracks} tracks`;
  switch (verifyState(g)) {
    case "verified":
      return (
        `This copy is identical, bit for bit, to the Redump dump "${g.name}": ${t} have the same size and SHA-1 ` +
        "checksum. It is a complete, unmodified image of the original disc: no read errors, no patches, translations " +
        "or cracks, no missing audio tracks."
      );
    case "mismatch":
      return (
        `The disc was identified as "${g.name}" (from its serial or track sizes), but its checksums differ from ` +
        "Redump's. The image may have read errors, be modified (patched, translated, cracked), or use another layout: " +
        "all tracks merged into one BIN, an ISO without the audio tracks, or a different pressing. It may still play " +
        "fine; the tracks below show where it differs."
      );
    case "unidentified":
      return (
        "No Redump entry matches this disc's serial, track sizes or checksums. It may be a homebrew or unreleased " +
        "disc, a modified image, or a dump in an unusual layout. It may still play fine."
      );
    case "pending":
      return "The server is computing the checksums of every track in the background; reload the page in a while.";
    case "unreadable":
      return `The server could not read the disc: ${g.error ?? "unknown error"}.`;
  }
}

// --- BIOS ----------------------------------------------------------------------

/** "05/10/95" (mm/dd/yy, from Sony ROMs) → "1995-05-10"; ISO dates as they are. */
export function biosDate(b: Bios): string | null {
  if (/^\d{4}-\d{2}-\d{2}$/.test(b.date ?? "")) return b.date;
  const m = /^(\d\d)\/(\d\d)\/(\d\d)$/.exec(b.date ?? "");
  if (!m) return null;
  const y = Number(m[3]) < 70 ? `20${m[3]}` : `19${m[3]}`;
  return `${y}-${m[1]}-${m[2]}`;
}

/** Console models from Redump's description: retail (SCPH) and debugging (DTL). */
export function biosModels(b: Bios): { retail: string[]; debug: string[] } {
  const list = (b.model ?? "").split(" (")[0]?.split("/").filter(Boolean) ?? [];
  return { retail: list.filter((m) => m.startsWith("SCPH")), debug: list.filter((m) => m.startsWith("DTL")) };
}

export function biosName(b: Bios): string {
  if (b.variant) {
    if (!b.version) return b.variant;
    return `${b.variant} · ${/^\d/.test(b.version) ? "v" : ""}${b.version}`;
  }
  const { retail } = biosModels(b);
  const base = retail.length ? retail.join(", ") : b.file.split("/").pop() ?? b.file;
  return `${base} · v${b.version ?? "?"}`;
}

/** What a BIOS is, for grouping. */
export function biosKind(b: Bios): "Console" | "PSP" | "OpenBIOS" | "OpenPS2BIOS" | "Other" {
  if (b.variant === "OpenBIOS") return "OpenBIOS";
  if (b.variant === "OpenPS2BIOS") return "OpenPS2BIOS";
  if (b.variant?.startsWith("PSP")) return "PSP";
  return b.variant ? "Other" : "Console";
}

/** The newest verified BIOS of each region, else the newest at all. */
export function preferredBios(list: Bios[]): Map<Bios["region"], Bios> {
  const best = new Map<Bios["region"], Bios>();
  for (const b of list) {
    const cur = best.get(b.region);
    const better =
      !cur ||
      (b.verified === true && cur.verified !== true) ||
      (b.verified === cur.verified && (biosDate(b) ?? "") > (biosDate(cur) ?? ""));
    if (better) best.set(b.region, b);
  }
  return best;
}

/** What sets known console BIOSes apart, by SHA-1 (a version and region can
 * have several images, e.g. v2.2 J: the SCPH-5000's and the SCPH-5903's).
 * Sources and details: docs/ps1-bios.md. */
const BIOS_NOTES: Record<string, string> = {
  // v1.0 J, SCPH-1000
  "343883a7b555646da8cee54aadd2795b6e7dd070":
    "The launch BIOS of 1994, with the first kernel: its exception handler has a bug that later versions fixed, and it lacks " +
    "functions some later games call. Its CD player only opens with an audio CD in the drive. The least compatible: for " +
    "testing launch-era behaviour.",
  // v2.0 E, SCPH-1002
  "20b98f3d80f11cbf5a7bfd0779b0e63760ecc62c":
    "The earliest European BIOS Redump lists, still with the 1994 kernel and its bugs: less compatible than later ones.",
  // v2.2 A, SCPH-1001
  "10155d8d6e6e832d6ea66db9bc098321fb5e8ebf":
    "Has the 1995 kernel that almost every later BIOS kept, so it is very compatible. The North American BIOS older " +
    "emulators such as ePSXe long recommended.",
  // v2.2 J, SCPH-5000
  ffa7f9a7fb19d773a0c3985a541c8e5623d2c30d:
    "The Japanese counterpart of the SCPH-1001's v2.2: the same 1995 kernel and menu. libretro lists another image under " +
    "the name scph5000.bin, so RetroArch cores that check it by checksum reject this one.",
  // v3.0 J, SCPH-5500
  b05def971d8ec59f346f2d9ac21fb742e3eb6917: "The Japanese BIOS Beetle PSX and SwanStation (libretro) expect, with the 1995 kernel.",
  // v3.0 A, SCPH-5501, SCPH-5503, SCPH-7003
  "0555c6fae8906f3f09baf5988f00e55f88e9f30b":
    "The North American BIOS Beetle PSX and SwanStation expect, with the 1995 kernel: recommended for North American " +
    "games. The SCPH-7003 shipped with it too, so despite its model number it has no Sound Scope.",
  // v3.0 E, SCPH-5502, SCPH-5552
  f6bc2d1f5eb6593de7d089c425ac681d6fffd3f0:
    "The European BIOS Beetle PSX and SwanStation expect, with the 1995 kernel: recommended for PAL games. It has the " +
    "older European menu, redesigned with the SCPH-7002.",
  // v4.0 J, SCPH-7000, SCPH-7500, SCPH-9000
  "77b10118d21ac7ffa9b35f9c4fd814da240eb3e9":
    "The only BIOS with the 1997 kernel, which differs from the 1995 one only in mapping 1 MB of ROM instead of 512 KB. " +
    "Adds Sound Scope, the CD player's music visualiser (SELECT during audio playback). Lacks the accented characters of " +
    "non-Japanese BIOSes.",
  // v4.1 A, SCPH-7001, SCPH-7501, SCPH-7503, SCPH-9001...
  "14df4f6c1e367ce097c11deae21566b4fe5647a9":
    "Back on the 1995 kernel, with Sound Scope, the CD player's music visualiser (SELECT during audio playback).",
  // v4.1 E, SCPH-7002, SCPH-7502, SCPH-9002
  "8d5de56a79954f29e9006929ba3fed9b6a418c1d":
    "The same code as the North American v4.1 but for its region letter: the 1995 kernel, Sound Scope, and the " +
    "redesigned menu shared with the North American and Japanese BIOSes.",
  // v4.5 A, SCPH-101
  dcffe16bd90a723499ad46c641424981338d8378:
    "From the PSone, the smaller redesign of 2000: the latest North American BIOS Redump lists, with the 1995 kernel.",
};

/** What is worth knowing about a BIOS, beyond its version, date, region
 * and models (shown next to it). */
export function biosDescription(b: Bios, preferred: boolean): string {
  const { retail } = biosModels(b);
  const parts: string[] = [];
  if (b.variant === "OpenBIOS") {
    parts.push(
      "The open-source replacement BIOS of the PCSX-Redux project (MIT licence). It boots discs of every region, " +
        "with its own boot animation instead of Sony's logo, and has no menu without a disc. A reimplementation: " +
        "some games may not run on it.",
    );
  } else if (b.variant) {
    parts.push(`The PS1 emulator of ${b.variant}, based on the Japanese version ${b.version ?? "?"}. It boots discs of every region.`);
    if (b.verifiedBy === "libretro") parts.push("Redump does not list it; it matches the checksum libretro lists.");
  } else {
    const region =
      b.region === "NTSC-U" ? "North American" : b.region === "PAL" ? "European and Australian" : b.region === "NTSC-J" ? "Japanese and Asian" : null;
    if (region) parts.push(`Boots ${region} discs.`);
    const note = BIOS_NOTES[b.sha1];
    if (note) parts.push(note);
    else if (b.version === "1.0") parts.push("The first BIOS, from the launch consoles of December 1994.");
    else if (retail.some((m) => /^SCPH-10[0-2]$/.test(m))) parts.push("From the PSone, the smaller redesign of 2000.");
    else if (b.version?.startsWith("4.")) parts.push("A late revision, from the last generation of the original console.");
  }
  if (b.verified === false) parts.push("Not a known-good dump: it may be patched or damaged.");
  if (preferred) {
    parts.push(
      b.region === "any"
        ? "The default for games of regions without a BIOS of their own."
        : `The newest ${b.verified ? "verified " : ""}BIOS of its region here: the default for its games.`,
    );
  }
  return parts.join(" ");
}

/** The same, for a PlayStation 2 BIOS. */
export function ps2BiosDescription(b: Bios): string {
  const { retail } = biosModels(b);
  const parts: string[] = [];
  if (b.variant === "OpenPS2BIOS") {
    parts.push(
      "An experimental open-source replacement for the PlayStation 2 BIOS, written for emulators (MIT licence). " +
        "A reimplementation in its early days: many games may not run on it.",
    );
  } else {
    const region: Partial<Record<Bios["region"], string>> = {
      "NTSC-U": "North American",
      PAL: "European and Australian",
      "NTSC-J": "Japanese",
      "NTSC-Asia": "Asian",
      "NTSC-China": "Chinese",
    };
    const name = region[b.region];
    if (name) parts.push(`Boots ${name} discs, and PlayStation discs of the same region.`);
    if (retail.some((m) => /^SCPH-7\d{4}$/.test(m))) parts.push("From the slim PlayStation 2 (SCPH-70000 series), released in 2004.");
    else if (retail.some((m) => /^SCPH-[1-3]\d{4}$/.test(m))) parts.push("From the original, larger PlayStation 2.");
  }
  if (b.verified === false) parts.push("Not a known-good dump: it may be patched or damaged.");
  return parts.join(" ");
}

export type Relevance = { level: "best" | "match" | "any" | "mismatch"; text: string };

export function biosRelevance(b: Bios, game: Game, preferred: Map<Bios["region"], Bios>): Relevance {
  const region = gameRegion(game);
  const verified = b.verified ? " Verified dump." : b.verified === false ? " Not a known-good dump." : "";
  if (b.region === "any") {
    // The default when no BIOS of the game's region is in the library.
    if (region && !preferred.has(region) && preferred.get("any")?.id === b.id) {
      return { level: "best", text: `Best match: region-free, and there is no ${region} BIOS in the library.${verified}` };
    }
    return { level: "match", text: `Region-free: boots discs of every region.${verified}` };
  }
  if (!region || b.region === "unknown") {
    return { level: "any", text: `The game's region is unknown: any BIOS may work.${verified}` };
  }
  if (b.region !== region) {
    return {
      level: "mismatch",
      text: `Region mismatch: ${b.region} BIOS, ${region} game.`,
    };
  }
  if (preferred.get(region)?.id === b.id) {
    return { level: "best", text: `Best match: same region (${region}), newest in the library.${verified}` };
  }
  return { level: "match", text: `Same region (${region}).${verified}` };
}

export function biosGames(b: Bios, games: Game[]): number {
  const own = games.filter((g) => g.console === b.console);
  return b.region === "any" ? own.length : own.filter((g) => gameRegion(g) === b.region).length;
}
