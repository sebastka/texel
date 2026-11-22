import "./style.css";
import { renderAccount, renderRegister, renderSignIn } from "./account";
import { renderAdmin } from "./admin";
import { api, gameRegion, image, type Bios, type Console, type Game, type GameDetail, type GameListItem, type SaveState, type User } from "./api";
import { renderCards } from "./cards";
import {
  REDUMP_EXPLAINED,
  VERIFY_LABELS,
  biosDate,
  biosDescription,
  biosGames,
  biosKind,
  biosModels,
  biosName,
  biosRelevance,
  firstSentence,
  formatDate,
  formatDateTime,
  formatSize,
  parseTags,
  preferredBios,
  ps2BiosDescription,
  regionName,
  verifyExplanation,
  verifyState,
  year,
  type VerifyState,
} from "./meta";
import { controlsDiagram } from "./controls";
import { h, type Child } from "./dom";
import { gamepadChip } from "./gamepad-ui";
import { Player } from "./player";
import type { NetStatus } from "./protocol";
import { roomPanel, saveName, savedName, setAccountName } from "./room";

const app = document.querySelector<HTMLDivElement>("#app")!;

const BIOS_KEY = "texel.bios";

/** PlayStation BIOSes: the ones games can be played with. */
let biosList: Bios[] = [];
/** PlayStation 2 BIOSes: listed, not used yet. */
let ps2BiosList: Bios[] = [];
let games: GameListItem[] = [];
/** The signed-in user: null shows the sign-in page (but to join a room). */
let me: User | null = null;
let player: Player | null = null;

function external(href: string, text: string) {
  return h("a.external", { href, target: "_blank", rel: "noopener noreferrer" }, text);
}

function stopPlayer() {
  player?.stop();
  player = null;
}

/** The console tab last shown, for the header's links. */
let shownConsole: Console = "ps1";

const CONSOLES: { id: Console; name: string }[] = [
  { id: "ps1", name: "PlayStation" },
  { id: "ps2", name: "PlayStation 2" },
];

const parseConsole = (s: string | undefined): Console => (s === "ps1" || s === "ps2" ? s : shownConsole);

export type Page = "games" | "bios" | "cards" | "join" | "admin" | "account";

function header(active: Page | null, ...right: Node[]) {
  const tab = (name: Page, href: string, label: string) => h(`a.tab${active === name ? ".active" : ""}`, { href }, label);
  const tabs = me
    ? [
        tab("games", `#/games/${shownConsole}`, "Games"),
        tab("bios", `#/bios/${shownConsole}`, "BIOS"),
        tab("cards", "#/cards", "Memory cards"),
        tab("join", "#/join", "Join"),
        ...(me.role === "admin" ? [tab("admin", "#/admin", "Admin")] : []),
      ]
    : [tab("join", "#/join", "Join")];
  const account = me
    ? h(
        "div.account",
        {},
        h("a.account-name", { href: "#/account", title: "Your account" }, me.name),
        h("button.ghost.small-button", { onclick: () => void signOut() }, "Sign out"),
      )
    : h("a.button.ghost.small-button", { href: "#/signin" }, "Sign in");
  return h(
    "header",
    {},
    h("a.brand", { href: me ? `#/games/${shownConsole}` : "#/signin" }, h("span.logo", {}, ""), "Texel"),
    h("nav", {}, ...tabs),
    h("div.spacer"),
    ...right,
    account,
  );
}

async function signOut() {
  stopPlayer();
  try {
    const { redirect } = await api.logout();
    me = null;
    setAccountName(null);
    if (redirect) location.href = redirect;
    else location.hash = "#/signin";
  } catch (e) {
    alert(`Signing out failed: ${String(e)}`);
  }
}

/** What the account, card and admin pages need from here. */
export interface Shell {
  app: HTMLElement;
  header: (active: Page | null, ...right: Node[]) => HTMLElement;
  me: () => User | null;
  /** After signing in or out: reloads the lists and shows `hash`. */
  signedIn: (user: User | null, hash?: string) => void;
  games: () => GameListItem[];
  stopPlayer: () => void;
}

const shell: Shell = {
  app,
  header,
  me: () => me,
  signedIn: (user, hash = `#/games/${shownConsole}`) => {
    me = user;
    setAccountName(user?.name ?? null);
    // Out of the registration page's own address.
    if (location.pathname !== "/" || location.search) history.replaceState(null, "", `/${hash}`);
    void load().then(() => {
      if (location.hash === hash) route();
      else location.hash = hash;
    });
  },
  games: () => games,
  stopPlayer,
};

/** One tab per console, under a page's title: `#/games/ps2`, `#/bios/ps1`... */
function consoleTabs(page: "games" | "bios", active: Console, count: (c: Console) => number) {
  return h(
    "nav.subtabs",
    { "aria-label": "Console" },
    ...CONSOLES.map(({ id, name }) =>
      h(
        `a.subtab${id === active ? ".active" : ""}`,
        id === active ? { href: `#/${page}/${id}`, "aria-current": "page" } : { href: `#/${page}/${id}` },
        name,
        h("span.subtab-count", {}, String(count(id))),
      ),
    ),
  );
}

/** Games of a console, one entry per multi-disc game. */
const gamesOf = (c: Console) => games.filter((g) => g.console === c && (!g.discs.length || g.discs[0]?.id === g.id));

const VERIFY_CLASS: Record<VerifyState, string> = {
  verified: "good",
  mismatch: "warn",
  unidentified: "warn",
  pending: "muted",
  unreadable: "bad",
};

function verifyBadge(g: Game) {
  const state = verifyState(g);
  const title =
    state === "verified"
      ? "Bit-for-bit identical to the Redump dump: a complete, unmodified image of the original disc."
      : state === "mismatch"
        ? "Checksums differ from Redump: damaged, modified, or another dump layout."
        : state === "unidentified"
          ? "No Redump entry matches this disc."
          : state === "pending"
            ? "Checksums are being computed in the background."
            : (g.error ?? "");
  return h(`span.badge.${VERIFY_CLASS[state]}`, { title }, VERIFY_LABELS[state]);
}

function biosBadge(b: Bios) {
  if (b.variant === "OpenBIOS") {
    return h("span.badge.muted", { title: "Built from source by the PCSX-Redux project: no list of checksums exists for it" }, "Open source");
  }
  if (b.variant === "OpenPS2BIOS") {
    return h("span.badge.muted", { title: "Built from source by the OpenPS2BIOS project: no list of checksums exists for it" }, "Open source");
  }
  if (b.verified === true) {
    const by =
      b.verifiedBy === "libretro"
        ? "Matches a known-good dump listed by libretro (Redump does not cover it)"
        : "Matches a dump in Redump's BIOS DAT";
    return h("span.badge.good", { title: by }, "Verified");
  }
  if (b.verified === false) return h("span.badge.warn", { title: "Not in Redump's BIOS DAT: modified or bad dump" }, "Unknown dump");
  return h("span.badge.muted", { title: "Redump's BIOS DAT is not downloaded yet" }, "Unchecked");
}

function cover(g: Game, kind: "boxart" | "title" | "snap" = "boxart", cls = "cover") {
  const fallback = h("div.cover-fallback", {}, kind === "boxart" ? g.title : "");
  const img = h(`img.${cls}`, { src: image(g.id, kind), alt: "", loading: "lazy", decoding: "async" });
  img.addEventListener("error", () => img.replaceWith(fallback));
  return img;
}

// --- BIOS choice ------------------------------------------------------------------

function rememberedBios(): string | null {
  try {
    return localStorage.getItem(BIOS_KEY);
  } catch {
    return null;
  }
}

function rememberBios(id: string) {
  try {
    localStorage.setItem(BIOS_KEY, id);
  } catch {
    // Private mode: not remembered.
  }
}

/** The BIOS images of a game's console. */
function biosesFor(game?: Game): Bios[] {
  return game?.console === "ps2" ? ps2BiosList : biosList;
}

/** The remembered BIOS when it suits the game, else the preferred one of its region. */
function defaultBios(game?: Game): Bios | undefined {
  const list = biosesFor(game);
  const remembered = list.find((b) => b.id === rememberedBios());
  const region = game ? gameRegion(game) : null;
  if (remembered && (!region || remembered.region === region || remembered.region === "any")) return remembered;
  const preferred = preferredBios(list);
  return (region ? preferred.get(region) : undefined) ?? preferred.get("any") ?? remembered ?? list[0];
}

// --- Games list ---------------------------------------------------------------------

type SortKey = "title" | "year" | "genre" | "developer" | "region" | "verified";
let sortKey: SortKey = "title";
let sortAsc = true;
let searchText = "";

function sortValue(g: GameListItem, key: SortKey): string {
  const d = g.details;
  switch (key) {
    case "title":
      return g.title.toLowerCase();
    case "year":
      return d?.releaseDate ?? "9999";
    case "genre":
      return (d?.genres[0] ?? "~").toLowerCase();
    case "developer":
      return (d?.developers[0] ?? "~").toLowerCase();
    case "region":
      return parseTags(g.tags).regions.join() || "~";
    case "verified":
      return verifyState(g);
  }
}

function matches(g: GameListItem, needle: string): boolean {
  if (!needle) return true;
  const d = g.details;
  const hay = [g.title, g.serial ?? "", ...g.tags, ...(d?.genres ?? []), ...(d?.developers ?? []), ...(d?.publishers ?? []), year(d?.releaseDate)];
  return hay.some((s) => s.toLowerCase().includes(needle));
}

// Filters, under the console tabs: a value per field, per console.
type FilterKey = "year" | "genre" | "developer" | "publisher" | "region" | "dump";
const FILTERS: { key: FilterKey; label: string }[] = [
  { key: "year", label: "Released" },
  { key: "genre", label: "Genre" },
  { key: "developer", label: "Developer" },
  { key: "publisher", label: "Publisher" },
  { key: "region", label: "Region" },
  { key: "dump", label: "Dump" },
];
/** What a game lacks a value for, as a filter value. */
const UNKNOWN = "";
const filters: Record<Console, Partial<Record<FilterKey, string>>> = { ps1: {}, ps2: {} };

/** A game's values for a filter (none: unknown). */
function filterValues(g: GameListItem, key: FilterKey): string[] {
  const d = g.details;
  switch (key) {
    case "year":
      return year(d?.releaseDate) ? [year(d?.releaseDate)] : [];
    case "genre":
      return d?.genres ?? [];
    case "developer":
      return d?.developers ?? [];
    case "publisher":
      return d?.publishers ?? [];
    case "region":
      return parseTags(g.tags).regions;
    case "dump":
      return [VERIFY_LABELS[verifyState(g)]];
  }
}

function passes(g: GameListItem, active: Partial<Record<FilterKey, string>>, except?: FilterKey): boolean {
  return FILTERS.every(({ key }) => {
    const want = active[key];
    if (want === undefined || key === except) return true;
    const values = filterValues(g, key);
    return want === UNKNOWN ? !values.length : values.includes(want);
  });
}

function renderGames(platform: Console = shownConsole) {
  stopPlayer();
  shownConsole = platform;
  const tbody = h("tbody");
  const count = h("span.count");
  const active = filters[platform];
  const filterBar = h("div.filters", { role: "group", "aria-label": "Filters" });
  // The filters' choices: the values of the games the search and the other
  // filters let through, with how many games have each.
  const fillFilters = (searched: GameListItem[]) => {
    const selects = FILTERS.map(({ key, label }) => {
      const counts = new Map<string, number>();
      for (const g of searched.filter((g) => passes(g, active, key))) {
        const values = filterValues(g, key);
        for (const v of values.length ? new Set(values) : [UNKNOWN]) counts.set(v, (counts.get(v) ?? 0) + 1);
      }
      const values = [...counts.keys()].filter((v) => v !== UNKNOWN);
      values.sort(key === "year" ? (a, b) => b.localeCompare(a) : (a, b) => a.localeCompare(b));
      if (counts.has(UNKNOWN)) values.push(UNKNOWN);
      const chosen = active[key];
      // A choice no game has any more stays listed, with none.
      if (chosen !== undefined && !counts.has(chosen)) values.push(chosen);
      const select = h(
        "select.filter",
        {
          "aria-label": label,
          class: chosen !== undefined ? "filter active" : "filter",
          onchange: (e: Event) => {
            const v = (e.target as HTMLSelectElement).value;
            if (v === "*") delete active[key];
            else active[key] = v.slice(1);
            fill();
          },
        },
        h("option", { value: "*", selected: chosen === undefined }, `${label}: any`),
        ...values.map((v) =>
          h(
            "option",
            { value: `=${v}`, selected: v === chosen },
            `${v === UNKNOWN ? "Unknown" : v} (${counts.get(v) ?? 0})`,
          ),
        ),
      );
      return select;
    });
    const any = Object.keys(active).length > 0;
    const clear = h(
      "button.ghost.small",
      {
        hidden: !any,
        onclick: () => {
          for (const { key } of FILTERS) delete active[key];
          fill();
        },
      },
      "Clear filters",
    );
    filterBar.replaceChildren(...selects, clear);
  };
  const fill = () => {
    const needle = searchText.trim().toLowerCase();
    const searched = gamesOf(platform).filter((g) => matches(g, needle));
    fillFilters(searched);
    const shown = searched
      .filter((g) => passes(g, active))
      .sort((a, b) => {
        const c = sortValue(a, sortKey).localeCompare(sortValue(b, sortKey));
        return (sortAsc ? c : -c) || a.title.localeCompare(b.title);
      });
    const total = gamesOf(platform).length;
    count.textContent = `${shown.length} of ${total} game${total === 1 ? "" : "s"}`;
    if (!shown.length) {
      tbody.replaceChildren(
        h("tr", {}, h("td.empty", { colSpan: 7 }, total ? "No game matches the search and filters." : `No games in the library's ${platform}/games/ directory.`)),
      );
      return;
    }
    tbody.replaceChildren(...shown.map(gameRow));
  };
  const th = (key: SortKey, label: string, cls = "") => {
    const active = key === sortKey;
    return h(
      "th",
      {
        class: `sortable${active ? " sorted" : ""}${cls ? ` ${cls.slice(1)}` : ""}`,
        onclick: () => {
          if (sortKey === key) sortAsc = !sortAsc;
          else [sortKey, sortAsc] = [key, true];
          renderGames(platform);
        },
        "aria-sort": active ? (sortAsc ? "ascending" : "descending") : "none",
      },
      label,
      active ? h("span.arrow", {}, sortAsc ? " ▲" : " ▼") : null,
    );
  };
  const search = h("input.search", {
    type: "search",
    placeholder: "Search title, genre, developer, serial…",
    value: searchText,
    oninput: (e: Event) => {
      searchText = (e.target as HTMLInputElement).value;
      fill();
    },
  });
  const rescan = h(
    "button.ghost",
    {
      title: "Rescan the library for new or removed files",
      onclick: async () => {
        // The server answers once the scan is done (seconds, on network
        // storage).
        rescan.disabled = true;
        rescan.textContent = "Rescanning…";
        try {
          await api.rescan();
          await load();
          renderGames(platform);
        } finally {
          rescan.disabled = false;
          rescan.textContent = "Rescan";
        }
      },
    },
    "Rescan",
  ) as HTMLButtonElement;
  const table = h(
    "table.games",
    {},
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th.col-cover", {}, ""),
        th("title", "Game"),
        th("year", "Released", ".col-year"),
        th("genre", "Genre", ".col-genre"),
        th("developer", "Developer", ".col-dev"),
        th("region", "Region", ".col-region"),
        th("verified", "Dump", ".col-verified"),
      ),
    ),
    tbody,
  );
  fill();
  app.replaceChildren(
    header("games", search, ...(me?.role === "admin" ? [rescan] : [])),
    h(
      "main.page",
      {},
      h(
        "div.page-head",
        {},
        h("h1", {}, "Games"),
        count,
      ),
      consoleTabs("games", platform, (c) => gamesOf(c).length),
      filterBar,
      platform === "ps2"
        ? h("p.muted.small.console-note", {}, "The PlayStation 2 emulator is in progress: many games do not get far yet.")
        : null,
      table,
    ),
  );
  search.focus();
}

function gameRow(g: GameListItem) {
  const d = g.details;
  const tags = parseTags(g.tags);
  const href = `#/game/${g.id}`;
  const description = firstSentence(d?.summary) || (d?.tagline ? `${d.tagline[0]?.toUpperCase()}${d.tagline.slice(1)}.` : "");
  return h(
    "tr",
    { onclick: (e: MouseEvent) => !(e.target as Element).closest("a") && (location.hash = href) },
    // PS2 covers are DVD cases: taller than wide.
    h("td.col-cover", {}, h(g.console === "ps2" ? "div.thumb.tall" : "div.thumb", {}, cover(g))),
    h(
      "td",
      {},
      h("a.game-title", { href }, g.title),
      tags.other.length || g.discs.length
        ? h(
            "span.small-tags",
            {},
            ...tags.other.filter((t) => !t.tag.startsWith("Disc ")).map((t) => h("span.tag", { title: t.note ?? "" }, t.tag)),
            g.discs.length ? h("span.tag", {}, `${g.discs.length} discs`) : null,
          )
        : null,
      g.serial ? h("div.serial", {}, g.serial) : null,
      description ? h("div.desc", {}, description) : null,
    ),
    h("td.col-year", {}, year(d?.releaseDate)),
    h("td.col-genre", {}, d?.genres.join(", ") ?? ""),
    h("td.col-dev", { title: d?.publishers.length ? `Published by ${d.publishers.join(", ")}` : "" }, d?.developers.join(", ") ?? ""),
    h(
      "td.col-region",
      {},
      h("div", {}, tags.regions.join(", ") || "—"),
      tags.languages.length ? h("div.muted.small", { title: tags.languages.join(", ") }, `${tags.languages.length} language${tags.languages.length > 1 ? "s" : ""}`) : null,
    ),
    h("td.col-verified", {}, verifyBadge(g)),
  );
}

// --- Game page ------------------------------------------------------------------------

function fact(label: string, value: Child | string[] | null | undefined) {
  if (value === null || value === undefined || value === "" || (Array.isArray(value) && !value.length)) return null;
  const text = Array.isArray(value) ? value.join(", ") : value;
  return h("div.fact", {}, h("dt", {}, label), h("dd", {}, text));
}

async function renderGame(id: string) {
  stopPlayer();
  app.replaceChildren(header("games"), h("main.page", {}, h("p.empty", {}, "Loading…")));
  let g: GameDetail;
  try {
    g = await api.game(id);
  } catch (e) {
    app.replaceChildren(header("games"), h("main.page", {}, h("p.error", {}, String(e))));
    return;
  }
  shownConsole = g.console;

  const d = g.details;
  const tags = parseTags(g.tags);
  const size = g.files.reduce((a, f) => a + f.size, 0);
  const sources = [
    d?.wikipediaUrl ? external(d.wikipediaUrl, "Wikipedia") : null,
    d?.igdbUrl ? external(d.igdbUrl, "IGDB") : null,
    external(`http://redump.org/discs/quicksearch/${encodeURIComponent(g.serial ?? g.title)}/`, "Redump"),
    // From the game's YAML file.
    ...Object.entries(g.links).map(([name, url]) => external(url, name)),
  ].filter((x): x is HTMLAnchorElement => x !== null);

  const description = d?.summary
    ? h(
        "div.description",
        {},
        ...d.summary.split(/\n+/).map((p) => h("p", {}, p)),
        h("p.source", {}, "From ", d.wikipediaUrl && d.summarySource === "Wikipedia" ? external(d.wikipediaUrl, "Wikipedia") : (d.summarySource ?? "?"), d.summarySource === "Wikipedia" ? " (CC BY-SA)" : ""),
      )
    : h("p.muted", {}, "No description found for this game.");
  const overridesError = g.overridesError
    ? h("p.warn-text", {}, `The game's YAML file is not used: ${g.overridesError}`)
    : null;

  app.replaceChildren(
    header("games"),
    h(
      "main.page.game",
      {},
      h(
        "aside.game-side",
        {},
        h("div.detail-cover", {}, cover(g)),
        h("div.shots", {}, cover(g, "title", "shot"), cover(g, "snap", "shot")),
      ),
      h(
        "div.game-main",
        {},
        h("h1", {}, g.title),
        g.serial ? h("div.serial", {}, g.serial) : null,
        d?.tagline ? h("p.tagline", {}, `${d.tagline[0]?.toUpperCase()}${d.tagline.slice(1)}`) : null,
        h(
          "div.tags",
          {},
          ...tags.regions.map((r) => h("span.tag.region", {}, r)),
          ...tags.other.map((t) => h("span.tag", { title: t.note ?? "" }, t.tag)),
          verifyBadge(g),
        ),
        description,
        overridesError,
        h(
          "dl.facts",
          {},
          fact("Released", formatDate(d?.releaseDate)),
          fact("Genre", d?.genres),
          fact("Modes", d?.modes),
          fact("Developer", d?.developers),
          fact("Publisher", d?.publishers),
          fact("Rating", d?.rating ? `${d.rating.toFixed(0)}/100 (IGDB)` : null),
          fact("Languages", tags.languages),
          fact("Region", gameRegion(g) ? regionName(gameRegion(g)!) : null),
          fact(
            "Discs",
            g.discs.length
              ? h(
                  "span.links",
                  {},
                  ...g.discs.flatMap((d, i) => {
                    const link = d.id === g.id ? h("strong", {}, `Disc ${d.number}`) : h("a.external", { href: `#/game/${d.id}` }, `Disc ${d.number}`);
                    return i ? [" · ", link] : [link];
                  }),
                )
              : null,
          ),
          fact("Serial", g.serial),
          fact("Volume", g.volumeId),
          fact("Redump name", g.name),
          fact("Image", `${g.files.length} file${g.files.length > 1 ? "s" : ""}, ${formatSize(size)} · ${g.path}`),
          fact("Sources", sources.length ? h("span.links", {}, ...sources.flatMap((a, i) => (i ? [" · ", a] : [a]))) : null),
        ),
        playSection(g),
        verificationSection(g),
      ),
    ),
  );
}

function playSection(g: GameDetail) {
  const list = biosesFor(g);
  const preferred = preferredBios(list);
  // The game's own BIOS (last started with, or its save states'), else the usual choice.
  let selected = (list.some((b) => b.id === g.lastBios) ? g.lastBios : null) ?? defaultBios(g)?.id ?? "";
  const play = h(
    "button.primary",
    {
      disabled: !list.length || !!g.error,
      onclick: () => {
        rememberBios(selected);
        void api.setBios(g.id, selected).catch(() => undefined);
        location.hash = `#/play/${g.id}/${selected}`;
      },
    },
    "Play",
  );
  const rank = { best: 0, match: 1, any: 2, mismatch: 3 };
  const ranked = list
    .map((b) => ({ b, rel: biosRelevance(b, g, preferred) }))
    .sort((x, y) => rank[x.rel.level] - rank[y.rel.level] || (biosDate(y.b) ?? "").localeCompare(biosDate(x.b) ?? ""));
  const mismatches = ranked.some((r) => r.rel.level === "mismatch");
  const rows = ranked.map(({ b, rel }) => {
    const radio = h("input", { type: "radio", name: "bios", value: b.id, checked: b.id === selected });
    const row = h(
      `tr.${rel.level}`,
      {
        onclick: () => {
          selected = b.id;
          radio.checked = true;
          rows.forEach((r) => r.classList.toggle("selected", r === row));
        },
      },
      h("td.pick", {}, radio),
      h("td", {}, h("div.bios-name", {}, biosName(b)), h("div.muted.small", {}, b.file)),
      h("td.pick-region", {}, b.region === "any" ? "Region-free" : b.region),
      h("td", {}, biosBadge(b)),
      h("td.relevance", {}, rel.text),
    );
    if (b.id === selected) row.classList.add("selected");
    return row;
  });
  return h(
    "section.panel",
    {},
    h("div.panel-head", {}, h("h2", {}, "Play"), play),
    list.length
      ? h(
          "table.bios-pick",
          {},
          h(
            "thead",
            {},
            h(
              "tr",
              {},
              h("th.pick", {}, ""),
              h("th", {}, "BIOS"),
              h("th.pick-region", {}, "Region"),
              h("th.pick-dump", {}, "Dump"),
              h("th.pick-notes", {}, "For this game"),
            ),
          ),
          h("tbody", {}, ...rows),
        )
      : h("p.error", {}, `No BIOS in the library: add one to ${g.console}/bios/.`),
    g.console === "ps2"
      ? h(
          "p.muted.small",
          {},
          "The game starts directly: the console boots its BIOS, which then runs the game rather than its browser (no logo, " +
            "no region check). The PlayStation 2 emulator is in progress: many games do not get far yet. Its memory cards and " +
            "save states are kept by the server, as the PlayStation's (a new card is unformatted: the game offers to format it).",
        )
      : null,
    mismatches && g.console === "ps1"
      ? h(
          "p.muted.small",
          {},
          "Greyed out: BIOS images of another region. A real console refuses discs of another region. Here, some combinations " +
            "boot anyway (North American BIOSes boot European discs), but Japanese BIOSes still refuse European discs; some " +
            "games also check the region themselves, and PAL and NTSC games expect different video timings (50 and 60 Hz). " +
            "A region-free BIOS avoids all this.",
        )
      : null,
  );
}

function verificationSection(g: GameDetail) {
  const state = verifyState(g);
  const short = (s: string | null) => (s ? h("code", { title: s }, `${s.slice(0, 10)}…`) : h("span.muted", {}, "—"));
  const mark = (m: boolean | null) =>
    m === true ? h("span.ok", { title: "Matches" }, "✓") : m === false ? h("span.ko", { title: "Differs" }, "✗") : h("span.muted", {}, "…");
  const countNote =
    g.redumpTracks !== null && g.redumpTracks !== g.files.length
      ? h("p.warn-text", {}, `Redump lists ${g.redumpTracks} tracks for this disc; this image has ${g.files.length} file${g.files.length > 1 ? "s" : ""}.`)
      : null;
  return h(
    "section.panel",
    {},
    h("div.panel-head", {}, h("h2", {}, "Dump verification"), verifyBadge(g)),
    h("p", {}, verifyExplanation(g)),
    h("p.muted.small", {}, REDUMP_EXPLAINED),
    countNote,
    g.sbi
      ? h(
          "p.small",
          {},
          h("strong", {}, "LibCrypt: "),
          "this disc's copy protection checks subchannel data that disc images do not keep. " +
            "Its SBI file, from Redump, provides it: the game runs as from the original disc.",
        )
      : null,
    h(
      "details.tracks",
      { open: state === "mismatch" },
      h("summary", {}, `Tracks (${g.files.length})`),
      h(
        "table.tracks",
        {},
        h("thead", {}, h("tr", {}, h("th", {}, "File"), h("th", {}, "Size"), h("th", {}, "SHA-1"), h("th", {}, "Redump track"), h("th", {}, "Redump SHA-1"), h("th", {}, ""))),
        h(
          "tbody",
          {},
          ...g.files.map((f) =>
            h(
              "tr",
              {},
              h("td", {}, f.name),
              h("td.num", {}, formatSize(f.size)),
              h("td", {}, short(f.sha1)),
              h("td.muted", {}, f.redumpName ?? "—"),
              h("td", {}, short(f.redumpSha1)),
              h("td", {}, mark(f.matches)),
            ),
          ),
        ),
      ),
    ),
  );
}

// --- BIOS page ------------------------------------------------------------------------

const biosCount = (c: Console) => (c === "ps1" ? biosList : ps2BiosList).length;

/** What the BIOS page says about each console: its cards, its groups. */
const BIOS_PAGES: Record<
  Console,
  { cards: [string, ...(string | [string, string][])[]][]; empty: string; regions: Bios["region"][]; repo: [string, string] }
> = {
  ps1: {
    cards: [
      [
        "What it is",
        "The console's firmware: it starts the console, shows the Sony logo, checks the disc and boots it. Games also use it for memory cards, controllers and CD access.",
        "The emulator needs a real one, dumped from a console.",
      ],
      [
        "Regions",
        "Each region has its own BIOS, and a console only boots discs of its region:",
        [
          ["NTSC-U", "North America (USA, Canada). 60 Hz; SLUS and SCUS discs."],
          ["PAL", "Europe, Australia and other PAL countries. 50 Hz; SLES and SCES discs."],
          ["NTSC-J", "Japan and Asia. 60 Hz; SLPS, SLPM and SCPS discs."],
        ],
        "Game pages pick a BIOS of the game's region, the newest one by default. The PSP's emulator BIOS and OpenBIOS are region-free.",
      ],
      [
        "Versions",
        "Sony revised the BIOS over the console's life. What matters most is its kernel, the code games call into: the 1994 one (v1.0 and v2.0) has bugs and lacks functions later games use; the 1995 one (v2.2 onwards) is the most compatible.",
        "For playing, any BIOS from v2.2 on of the game's region works. Later versions mostly changed the menu, and v4.0 added Sound Scope, the CD player's music visualiser.",
      ],
      [
        "Verified dumps",
        "A verified BIOS matches a dump listed by Redump (or libretro, for the PSP's): an unmodified copy of the original ROM.",
        "Unknown dumps may be patched, to skip the logo or region checks, or damaged. OpenBIOS is built from source: no list of checksums exists for it.",
      ],
    ],
    empty: "No BIOS found: put 512 KiB BIOS images in the library's ps1/bios/ directory.",
    regions: ["NTSC-U", "PAL", "NTSC-J", "any", "unknown"],
    repo: ["PCSX-Redux", "https://github.com/grumpycoders/pcsx-redux/commit/"],
  },
  ps2: {
    cards: [
      [
        "What it is",
        "The PlayStation 2's firmware: 4 MiB, with the console's kernel, its menus and browser, the I/O processor's drivers, and what runs PlayStation discs.",
        "The emulator boots a real one, dumped from a console, then starts the game directly. A BIOS can also be started on its own, to its browser.",
      ],
      [
        "Regions",
        "As for the PlayStation, each region has its own BIOS:",
        [
          ["NTSC-U", "North America. SLUS and SCUS discs."],
          ["PAL", "Europe, Australia and other PAL countries. SLES and SCES discs."],
          ["NTSC-J", "Japan. SLPS, SLPM and SCPS discs."],
          ["NTSC-Asia, NTSC-China", "Other Asian models, with their own menus."],
        ],
        "Game pages pick a BIOS of the game's region, the newest one by default.",
      ],
      [
        "Versions",
        "Sony revised the BIOS for each hardware revision, from v1.00 (the Japanese launch model, 2000) to v2.30 (the last slim models, 2008). The models are listed by name: one image serves several.",
        "Games start directly, without the browser, so any version of the game's region should do; the newer, the more consoles' worth of fixes.",
      ],
      [
        "Verified dumps",
        "A verified BIOS matches a dump listed in Redump's PlayStation 2 BIOS DAT: an unmodified copy of the original ROM.",
        "Unknown dumps may be patched or damaged. OpenPS2BIOS, an open-source replacement, is built from source: no list of checksums exists for it.",
      ],
    ],
    empty: "No BIOS found: put 4 MiB PlayStation 2 BIOS images in the library's ps2/bios/ directory.",
    regions: ["NTSC-U", "PAL", "NTSC-J", "NTSC-Asia", "NTSC-China", "any", "unknown"],
    repo: ["OpenPS2BIOS", "https://github.com/TamaribaEmu/OpenPS2BIOS/commit/"],
  },
};

function renderBios(platform: Console = shownConsole) {
  stopPlayer();
  shownConsole = platform;
  const page = BIOS_PAGES[platform];
  const list = platform === "ps1" ? biosList : ps2BiosList;
  const preferred = preferredBios(list);
  const kinds = ["Console", "PSP", "OpenBIOS", "OpenPS2BIOS", "Other"] as const;
  const kindTitles: Record<(typeof kinds)[number], string> = {
    Console: "Console BIOSes, dumped from Sony hardware",
    PSP: "PSP",
    OpenBIOS: "OpenBIOS",
    OpenPS2BIOS: "OpenPS2BIOS",
    Other: "Other",
  };
  const sorted = [...list].sort(
    (a, b) =>
      kinds.indexOf(biosKind(a)) - kinds.indexOf(biosKind(b)) ||
      page.regions.indexOf(a.region) - page.regions.indexOf(b.region) ||
      (biosDate(a) ?? "").localeCompare(biosDate(b) ?? "") ||
      a.file.localeCompare(b.file),
  );
  const rows = sorted.flatMap((b, i) => {
    const kind = biosKind(b);
    const heading =
      i === 0 || biosKind(sorted[i - 1]!) !== kind ? [h("tr.group-row", {}, h("th", { colSpan: 7 }, kindTitles[kind]))] : [];
    return [...heading, biosRow(b)];
  });
  function biosRow(b: Bios) {
    const { retail, debug } = biosModels(b);
    const isPreferred = preferred.get(b.region)?.id === b.id;
    const n = biosGames(b, games);
    // The version has its own column.
    const name = b.variant ?? (retail.join(", ") || b.file.split("/").pop() || b.file);
    const defaultBadge = isPreferred
      ? h(
          "span.badge.accent",
          {
            title:
              b.region === "any"
                ? "Default BIOS for games of regions without a BIOS of their own"
                : "Default BIOS for games of this region",
          },
          "Default",
        )
      : null;
    // data-label: shown before the value when rows become cards (narrow screens).
    return h(
      "tr",
      {},
      h(
        "td.bios-cell",
        {},
        h("div.bios-name", {}, name, defaultBadge),
        h("div.muted.small", {}, b.file, debug.length ? ` · also ${debug.join(", ")}` : ""),
      ),
      h("td.nowrap.fact-cell", { "data-label": "Version" }, b.version ?? "—"),
      h("td.nowrap.fact-cell", { "data-label": "Date" }, formatDate(biosDate(b)) ?? "—"),
      h("td.nowrap.fact-cell", { title: regionName(b.region), "data-label": "Region" }, b.region === "any" ? "Region-free" : b.region),
      h("td.nowrap.fact-cell", {}, biosBadge(b)),
      h("td.num.fact-cell", { title: "Games in the library this BIOS boots", "data-label": "Games" }, String(n)),
      h(
        "td.relevance.desc-cell",
        {},
        platform === "ps1" ? biosDescription(b, isPreferred) : ps2BiosDescription(b),
        platform === "ps2" && !b.variant
          ? h("div", {}, h("a.button.ghost.small-button", { href: `#/console/${b.id}` }, "Start the console"))
          : null,
        platform === "ps1" && biosKind(b) === "Console"
          ? h("div", {}, h("a.button.ghost.small-button", { href: `#/console/${b.id}` }, "Start the console"))
          : null,
        b.commit && (platform === "ps1" || b.variant === "OpenPS2BIOS")
          ? h(
              "div.small",
              {},
              `Built from ${page.repo[0]} commit `,
              h("a.external.mono", { href: `${page.repo[1]}${b.commit}`, target: "_blank", rel: "noopener noreferrer" }, b.commit.slice(0, 7)),
              ".",
            )
          : null,
        h("div.sha", { title: `SHA-1 ${b.sha1} (click to copy)`, onclick: () => void navigator.clipboard?.writeText(b.sha1) }, `SHA-1 ${b.sha1.slice(0, 12)}…`),
      ),
    );
  }
  const card = ([title, ...content]: (typeof page.cards)[number]) =>
    h(
      "section.info-card",
      {},
      h("h2", {}, title),
      ...content.map((c) =>
        typeof c === "string"
          ? h("p", {}, c)
          : h("ul.regions", {}, ...c.map(([name, text]) => h("li", {}, h("strong", {}, name), " ", text))),
      ),
    );
  app.replaceChildren(
    header("bios"),
    h(
      "main.page",
      {},
      h("div.page-head", {}, h("h1", {}, "BIOS"), h("span.count", {}, `${list.length} image${list.length === 1 ? "" : "s"}`)),
      consoleTabs("bios", platform, biosCount),
      h("div.info-cards", {}, ...page.cards.map(card)),
      list.length
        ? h(
            "table.bios-list",
            {},
            h(
              "thead",
              {},
              h(
                "tr",
                {},
                h("th", {}, "BIOS"),
                h("th", {}, "Version"),
                h("th", {}, "Date"),
                h("th", {}, "Region"),
                h("th", {}, "Dump"),
                h("th.num", { title: "Games in the library this BIOS boots" }, "Games"),
                h("th", {}, "Notes"),
              ),
            ),
            h("tbody", {}, ...rows),
          )
        : h("p.error", {}, page.empty),
    ),
  );
}

// --- Player -----------------------------------------------------------------------------

/** Joining a friend's room: a form, or straight in from a link. */
function renderJoin(code: string | undefined) {
  stopPlayer();
  const error = h("p.error", { hidden: true });
  // Signed in: the account's name, as the room will show it.
  const name = h("input.name-input", {
    value: me ? me.name : savedName(),
    placeholder: "Your name",
    maxLength: 24,
    autocomplete: "nickname",
    "aria-label": "Your name",
    readOnly: !!me,
    title: me ? "Your account's name" : "",
  });
  const input = h("input.code-input", {
    value: code ?? "",
    placeholder: "ABC-DEF",
    autocomplete: "off",
    spellcheck: "false",
    "aria-label": "Room code",
  });
  const fail = (text: string) => {
    error.textContent = text;
    error.hidden = false;
  };
  const submit = () => {
    const normalised = input.value.toUpperCase().replace(/[^A-Z0-9]/g, "");
    const who = name.value.trim();
    if (!who) return name.focus();
    if (!normalised) return input.focus();
    if (!me) saveName(who);
    error.hidden = true;
    api.room(normalised).then(
      (room) => {
        if (room.full) fail("This room is full.");
        else renderPlayer(room.game, room.bios, { code: normalised, name: who });
      },
      () => fail("No open room has this code: ask for a new one."),
    );
  };
  app.replaceChildren(
    header("join"),
    h(
      "main.join",
      {},
      h("h1", {}, "Join a room"),
      h(
        "p.muted",
        {},
        "A friend opens a room from their game (Play together) and gives you its code. " +
          "The game then runs in your browser too, in step with theirs.",
      ),
      h(
        "form.join-form",
        {
          onsubmit: (e: Event) => {
            e.preventDefault();
            submit();
          },
        },
        h("label", {}, "Your name", name),
        h("label", {}, "Room code", input),
        h("button.primary", { type: "submit" }, "Join"),
      ),
      error,
    ),
  );
  // From a link, with a name already known: straight in.
  if (code && name.value.trim()) submit();
  else if (!name.value.trim()) name.focus();
  else input.focus();
}

/** Save states: a button opening a panel of slots (under the player's
 * header), and F5/F9 to save and load slot 1, until the page changes. */
function stateControls(id: string, biosId: string, bioses: Bios[], hidden = false) {
  const statesPanel = h("div.states-panel", { hidden: true });
  const renderStates = async () => {
    let saved: SaveState[] = [];
    try {
      saved = await api.states(id);
    } catch {
      // Shown as empty.
    }
    const slots = [1, 2, 3, 4].map((slot) => {
      const s = saved.find((x) => x.slot === slot);
      const when = s ? formatDateTime(new Date(s.savedAt * 1000)) : "Empty";
      // A state loads only with the BIOS it was saved with.
      const otherBios = s?.bios && s.bios !== biosId ? bioses.find((b) => b.id === s.bios) : undefined;
      const shot = s?.screenshot
        ? h("img.state-shot", { src: `/api/games/${encodeURIComponent(id)}/states/${slot}/screenshot?t=${s.savedAt}`, alt: "" })
        : h("div.state-shot.empty", {}, s ? "" : "Empty");
      return h(
        "div.state-slot",
        {},
        shot,
        h("div.state-meta", {}, h("strong", {}, `Slot ${slot}`), h("span.muted.small", {}, s ? when : "")),
        otherBios ? h("span.small.state-bios", {}, `Saved with ${biosName(otherBios)}`) : null,
        h(
          "div.state-actions",
          {},
          h("button.ghost", { onclick: () => player?.saveState(slot) }, "Save"),
          h(
            "button.ghost",
            {
              disabled: !s || !!otherBios,
              title: otherBios ? `Saved with another BIOS (${biosName(otherBios)}): start the game with it to load this state.` : undefined,
              onclick: () => player?.loadState(slot),
            },
            "Load",
          ),
        ),
      );
    });
    statesPanel.replaceChildren(
      ...slots,
      h("p.muted.small.states-help", {}, "F5 saves to slot 1, F9 loads it. States are kept per disc, on the server."),
    );
  };
  const statesButton = h(
    "button.ghost",
    {
      hidden,
      onclick: () => {
        statesPanel.hidden = !statesPanel.hidden;
        if (!statesPanel.hidden) void renderStates();
      },
    },
    "States",
  );
  const quickKeys = (e: KeyboardEvent) => {
    if ((e.code === "F5" || e.code === "F9") && !statesButton.disabled) {
      e.preventDefault();
      if (e.code === "F5") player?.saveState(1);
      else player?.loadState(1);
    }
  };
  window.addEventListener("keydown", quickKeys);
  window.addEventListener("hashchange", () => window.removeEventListener("keydown", quickKeys), { once: true });
  return { panel: statesPanel, button: statesButton, render: renderStates };
}

/** The fast-forward toggle of a player's bar. */
function fastToggle() {
  const button = h(
    "button.ghost.toggle",
    {
      title: "Fast-forward: runs the game as fast as this machine can, without sound. Off: the console's own speed.",
      "aria-pressed": "false",
      onclick: () => {
        const on = player?.toggleFast() ?? false;
        button.textContent = on ? "Fast-forward: on" : "Fast-forward: off";
        button.setAttribute("aria-pressed", String(on));
      },
    },
    "Fast-forward: off",
  ) as HTMLButtonElement;
  return button;
}

function renderPlayer(id: string, biosId: string, join: { code: string; name: string } | null = null) {
  stopPlayer();
  const game = games.find((g) => g.id === id);
  const bios = biosList.find((b) => b.id === biosId);
  if (game) document.title = `${game.title} · Texel`;
  if (game?.console === "ps2") {
    renderConsole(biosId, game);
    return;
  }
  const status = h("span.status", {}, "Starting…");
  const screen = h("div.screen");
  const pause = h("button.ghost", { onclick: () => player?.togglePause() }, "Pause");
  const smooth = h(
    "button.ghost.toggle",
    {
      title: "Smooth: blend pixels when enlarging the picture. Off: sharp, square pixels.",
      onclick: () => {
        if (player) setSmooth(player.toggleSmooth());
      },
    },
    "",
  );
  const analog = h(
    "button.ghost.toggle",
    {
      title: "The controller's Analog button (also the gamepad's Home button). Games may switch the mode themselves, or lock it.",
      onclick: () => player?.pressAnalog(),
    },
    "Analog: off",
  );
  const resolution = h(
    "button.ghost",
    {
      title:
        "Internal resolution: draws 3D at 2 to 4 times the console's resolution, for finer edges. " +
        "Costs CPU: lower it if the game slows down. FMVs and 2D pictures stay at native resolution.",
      onclick: () => {
        if (player) setResolution(player.cycleScale());
      },
    },
    "",
  );
  const setResolution = (scale: number) => (resolution.textContent = `Resolution: ${scale}×`);
  const setSmooth = (on: boolean) => {
    smooth.textContent = on ? "Smooth: on" : "Smooth: off";
    smooth.setAttribute("aria-pressed", String(on));
  };
  const reset = h("button.ghost", { hidden: !!join, onclick: () => player?.reset() }, "Reset");
  // Enhancements: precise geometry (GPU only), widescreen (16:9).
  const showToggle = (b: Element, label: string, on: boolean) => {
    b.textContent = `${label}: ${on ? "on" : "off"}`;
    b.setAttribute("aria-pressed", String(on));
  };
  const precise = h(
    "button.ghost.toggle",
    {
      title:
        "Precise geometry: draws 3D from the exact positions the console computed, before it rounded them: no wobbling " +
        "polygons, textures in perspective. Needs WebGPU.",
      onclick: () => {
        if (player) showToggle(precise, "Precise", player.togglePrecise());
      },
    },
    "",
  ) as HTMLButtonElement;
  const wide = h(
    "button.ghost.toggle",
    {
      title:
        "Widescreen: squeezes 3D so that the picture can be shown 16:9. Works with most 3D games; 2D (menus, text) is " +
        "stretched, and things may appear late at the sides. Not while playing together.",
      onclick: () => {
        if (player) setWide(player.setWidescreen());
      },
    },
    "",
  ) as HTMLButtonElement;
  const setWide = (on: boolean) => {
    showToggle(wide, "Widescreen", on);
    screen.classList.toggle("wide", on);
  };
  const fast = fastToggle();
  fast.hidden = !!join;
  // The disc in the drive, which changes with multi-disc games.
  let current = id;
  // Netplay: the room, beside the screen.
  let net: NetStatus | null = null;
  const room = roomPanel({
    guest: !!join,
    open: (name) => player?.hostNetplay(current, biosId, name),
    setPorts: (ports) => player?.setNetPorts(ports),
    setView: (view) => player?.setNetView(view),
    say: (text) => player?.say(text),
    leave: () => {
      player?.leaveNetplay();
      if (join) location.hash = `#/game/${id}`;
    },
  });
  room.element.hidden = !join;
  const netButton = h(
    "button.ghost.toggle",
    {
      hidden: !!join,
      title: "Play with friends: they run this game in their browser, in step with yours.",
      "aria-pressed": "false",
      onclick: () => {
        room.element.hidden = !room.element.hidden;
        netButton.setAttribute("aria-pressed", String(!room.element.hidden));
      },
    },
    "Play together",
  );
  const inRoom = () => !!net && net.state !== "ended";
  const onNet = (status: NetStatus) => {
    net = status;
    room.status(status);
    // What would break the members' sync: anything once others are in;
    // changing discs as soon as a room is open (members load its disc).
    const others = status.members.length > 1 && inRoom();
    const locked = !!join || others;
    reset.disabled = locked;
    statesButton.disabled = locked;
    // In step with the others: no fast-forward (the worker ignores it then).
    if (inRoom() && player?.fast) fast.click();
    fast.disabled = inRoom();
    // Widescreen changes what the console computes: all must agree.
    if (inRoom() && player?.widescreen) setWide(player.setWidescreen(false));
    wide.disabled = inRoom() || !!join;
    if (locked) statesPanel.hidden = true;
    if (discPicker) discPicker.disabled = inRoom();
  };
  const full = h("button.ghost", { onclick: () => void screen.requestFullscreen() }, "Fullscreen");
  // Save states: a panel of slots under the header.
  const { panel: statesPanel, button: statesButton, render: renderStates } = stateControls(id, biosId, biosList, !!join);
  const discs = game?.discs ?? [];
  const discPicker = discs.length && !join
    ? h(
        "select",
        {
          title: "Change discs: opens the console's lid and closes it on the chosen disc. Do it when the game asks.",
          onchange: (e: Event) => {
            const target = (e.target as HTMLSelectElement).value;
            current = target;
            void api.game(target).then((d) => player?.swapDisc(d));
          },
        },
        ...discs.map((d) => h("option", { value: d.id, selected: d.id === id }, `Disc ${d.number}`)),
      )
    : null;
  app.replaceChildren(
    header(null, h("span.now", { title: bios ? biosName(bios) : "" }, game?.title ?? "")),
    h(
      "main.player",
      {},
      h(
        "div.player-body",
        {},
        h(
          "div.player-stage",
          {},
          // Controls get their own bar, which wraps on narrow screens.
          h(
            "div.player-bar",
            {},
            status,
            h(
              "div.player-buttons",
              {},
              ...(discPicker ? [discPicker] : []),
              gamepadChip((on) => {
                if (player) player.inputSuspended = on;
              }),
              pause,
              fast,
              netButton,
              reset,
              statesButton,
              analog,
              resolution,
              precise,
              wide,
              smooth,
              full,
              h("a.button.ghost", { href: `#/game/${id}` }, "Quit"),
            ),
          ),
          statesPanel,
          screen,
        ),
        room.element,
        h(
          "details.controls",
          { open: true },
          h("summary", {}, "Controls"),
          controlsDiagram(),
          h(
            "p.controls-notes",
            {},
            h("span.keycap-inline", {}, "F5"),
            " saves a state to slot 1, ",
            h("span.keycap-inline", {}, "F9"),
            " loads it. Gamepads work too, with the standard layout; their Home button is the Analog button.",
          ),
        ),
      ),
    ),
  );
  player = new Player(screen, {
    onStatus: (s) => (status.textContent = s),
    onPause: (p) => (pause.textContent = p ? "Resume" : "Pause"),
    onStateSaved: () => {
      if (!statesPanel.hidden) void renderStates();
    },
    onStarted: (renderer) => {
      const gpu = !!renderer?.includes("WebGPU");
      precise.disabled = !gpu;
      if (!gpu) precise.title += " This browser has no WebGPU: drawn in software.";
    },
    onAnalog: (on) => {
      analog.textContent = on ? "Analog: on" : "Analog: off";
      analog.setAttribute("aria-pressed", String(on));
    },
    onNet,
    onChat: (line) => room.chat(line),
  });
  setSmooth(player.smooth);
  setResolution(player.scale);
  showToggle(precise, "Precise", player.precise);
  // A guest plays the host's game as it is.
  setWide(join ? player.setWidescreen(false) : player.widescreen);
  void api.game(id).then(
    (g) => player?.start(biosId, g, join),
    (e: unknown) => (status.textContent = String(e)),
  );
}

/** A console: a BIOS to its menu (with a game's memory cards, `cardsId`,
 * or blank ones), or for the PlayStation 2 with a game's disc. */
function renderConsole(biosId: string, game?: Game, cardsId: string | null = null) {
  stopPlayer();
  const bios = [...biosList, ...ps2BiosList].find((b) => b.id === biosId);
  const platform: Console = game?.console ?? bios?.console ?? "ps2";
  shownConsole = platform;
  const consoleName = platform === "ps1" ? "PlayStation" : "PlayStation 2";
  document.title = `${game ? game.title : consoleName} · Texel`;
  const status = h("span.status", {}, "Starting…");
  const screen = h("div.screen");
  const pause = h("button.ghost", { onclick: () => player?.togglePause() }, "Pause");
  const full = h("button.ghost", { onclick: () => void screen.requestFullscreen() }, "Fullscreen");
  // Save states, for games (they are kept per disc).
  const states = game ? stateControls(game.id, biosId, ps2BiosList) : null;
  app.replaceChildren(
    header(null, h("span.now", {}, game ? game.title : bios ? biosName(bios) : consoleName)),
    h(
      "main.player",
      {},
      h(
        "div.player-body",
        {},
        h(
          "div.player-stage",
          {},
          h(
            "div.player-bar",
            {},
            status,
            h(
              "div.player-buttons",
              {},
              gamepadChip((on) => {
                if (player) player.inputSuspended = on;
              }),
              pause,
              fastToggle(),
              ...(states ? [states.button] : []),
              full,
              h("a.button.ghost", { href: game ? `#/game/${game.id}` : `#/bios/${platform}` }, "Quit"),
            ),
          ),
          ...(states ? [states.panel] : []),
          ...(game ? [] : [cardsPicker(platform, biosId, cardsId)]),
          screen,
        ),
        h(
          "details.controls",
          { open: true },
          h("summary", {}, "Controls"),
          controlsDiagram(),
          platform === "ps2"
            ? h(
                "p.controls-notes",
                {},
                "The PlayStation 2 is in progress: its browser runs, and games start, but many do not get far yet. " +
                  "In a game, F5 saves a state to slot 1 and F9 loads it.",
              )
            : h(
                "p.controls-notes",
                {},
                "Without a disc, the PlayStation starts to its menu: its memory card manager and CD player. " +
                  "Gamepads work too, with the standard layout.",
              ),
        ),
      ),
    ),
  );
  player = new Player(screen, {
    onStatus: (s) => (status.textContent = s),
    onPause: (p) => (pause.textContent = p ? "Resume" : "Pause"),
    onStateSaved: () => {
      if (states && !states.panel.hidden) void states.render();
    },
    onAnalog: () => undefined,
  });
  if (!game) {
    void player.startConsole(platform, biosId, undefined, cardsId);
    return;
  }
  const p = player;
  void api.game(game.id).then(
    (detail) => p.startConsole(platform, biosId, detail),
    (e: unknown) => (status.textContent = `Cannot load the game: ${String(e)}`),
  );
}

/** Which memory cards a console started without a disc gets: a game's
 * (what is written goes to them), or blank ones, not kept. Changing
 * restarts the console. */
function cardsPicker(platform: Console, biosId: string, cardsId: string | null): HTMLElement {
  const ofConsole = games.filter((g) => g.console === platform && (!g.disc || g.disc === 1));
  const option = (g: GameListItem) =>
    h("option", { value: g.id, selected: g.id === cardsId }, g.serial ? `${g.title} (${g.serial})` : g.title);
  const saved = h("optgroup", { label: "Games with saved cards" });
  const others = h("optgroup", { label: "Other games" }, ...ofConsole.map(option));
  const select = h(
    "select",
    {
      onchange: () => {
        const id = select.value;
        location.hash = id ? `#/console/${biosId}/${encodeURIComponent(id)}` : `#/console/${biosId}`;
      },
    },
    h("option", { value: "", selected: !cardsId }, "Blank cards, not kept"),
    saved,
    others,
  );
  // The games with cards first (their own group); the list holds every
  // game meanwhile.
  void api.memcards().then(
    (ids) => {
      const withCards = new Set(ids);
      saved.replaceChildren(...ofConsole.filter((g) => withCards.has(g.id)).map(option));
      others.replaceChildren(...ofConsole.filter((g) => !withCards.has(g.id)).map(option));
      saved.hidden = !saved.children.length;
    },
    () => undefined,
  );
  saved.hidden = true;
  return h(
    "div.console-cards",
    {},
    h("label", {}, "Memory cards", select),
    h("span.muted.small", {}, cardsId ? "Saves written here go to this game's cards." : "Pick a game to manage its saves."),
  );
}

async function load() {
  me = await api.me();
  setAccountName(me?.name ?? null);
  if (!me) {
    // Only signed-in users see the library.
    [biosList, ps2BiosList, games] = [[], [], []];
    return;
  }
  let all: Bios[];
  [all, games] = await Promise.all([api.bios(), api.games()]);
  biosList = all.filter((b) => b.console === "ps1");
  ps2BiosList = all.filter((b) => b.console === "ps2");
}

/** Pages open without an account: signing in, registering, joining a room
 * (a room's code grants its game). */
const PUBLIC = new Set(["signin", "join"]);

function route() {
  const [, view, id, extra] = location.hash.split("/");
  // The player names its game.
  document.title = "Texel";
  // An invitation's link: /register?token=…
  if (location.pathname === "/register") {
    stopPlayer();
    renderRegister(shell, new URLSearchParams(location.search).get("token") ?? "");
    return;
  }
  if (!me && !PUBLIC.has(view ?? "")) {
    stopPlayer();
    renderSignIn(shell);
    return;
  }
  if (view === "signin") {
    stopPlayer();
    if (me) location.hash = `#/games/${shownConsole}`;
    else renderSignIn(shell, id ? decodeURIComponent(id) : undefined);
    return;
  }
  if (view === "cards") {
    stopPlayer();
    void renderCards(shell);
  } else if (view === "admin") {
    stopPlayer();
    void renderAdmin(shell);
  } else if (view === "account") {
    stopPlayer();
    renderAccount(shell);
  } else if (view === "game" && id) void renderGame(id);
  else if (view === "play" && id) renderPlayer(id, extra ?? defaultBios()?.id ?? "");
  else if (view === "console" && id) renderConsole(id, undefined, extra ? decodeURIComponent(extra) : null);
  else if (view === "bios") renderBios(parseConsole(id));
  else if (view === "games") renderGames(parseConsole(id));
  else if (view === "join") renderJoin(id);
  else renderGames();
  window.scrollTo(0, 0);
}

window.addEventListener("hashchange", () => {
  // Metadata may have arrived since: refresh the lists on navigation.
  void load().then(route, route);
});
load().then(route, (e: unknown) => app.replaceChildren(header(null), h("p.error", {}, `Cannot reach the server: ${String(e)}`)));
